//! The client half of the connection.
//!
//! Owns the socket, drives the handshake, and translates between wire messages
//! and [`GameState`]. If the link goes quiet it drops back to handshaking on its
//! own: because the server keys players by token rather than by address, coming
//! back returns the player to their own ore, power-ups, and vehicles rather than
//! to a fresh slot.

use std::io::ErrorKind;
use std::net::{SocketAddr, UdpSocket};
use std::time::{Duration, Instant};

use bevy::prelude::*;
use orewar_shared::bytes::{Decode, Encode};
use orewar_shared::net::{Endpoint, MAX_PACKET, PacketKind, begin_packet, parse_packet};
use orewar_shared::protocol::{
    ClientMessage, DenyReason, GameEvent, HarvesterMode, InputFrame, PROTOCOL_ID,
    ServerMessage,
};
use orewar_shared::sim;
use orewar_shared::world::PowerUp;

use crate::state::GameState;

/// How often an unanswered connection request is repeated.
const HANDSHAKE_RETRY: f64 = 0.25;
/// Silence after which the connection is considered lost and retried.
const TIMEOUT: f64 = 8.0;
/// How long an unanswered handshake runs before it is reported on the terminal.
/// UDP gives no failure to observe -- a wrong address, a firewall, and a server
/// that was never started all look identical to waiting -- so say so out loud
/// rather than sitting on a silent retry loop forever.
const HANDSHAKE_UNANSWERED: f64 = 4.0;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Link {
    Connecting,
    Connected,
    /// The server refused us; retrying will not help.
    Denied(DenyReason),
}

#[derive(Resource)]
pub struct NetClient {
    socket: UdpSocket,
    endpoint: Endpoint,
    pub link: Link,
    /// Stable identity, so a reconnect resumes the same player.
    token: u64,
    name: String,
    start: Instant,
    last_request: f64,
    /// When the current handshake attempt began, for the unanswered warning.
    handshake_started: f64,
    warned_unanswered: bool,
    pub server_addr_text: String,
}

impl NetClient {
    pub fn connect(server: SocketAddr, token: u64, name: String) -> std::io::Result<Self> {
        // Port 0: let the OS pick. Two clients on one machine must not fight
        // over a fixed port, which is exactly how you test this locally.
        let bind: SocketAddr =
            if server.is_ipv4() { "0.0.0.0:0".parse().unwrap() } else { "[::]:0".parse().unwrap() };
        let socket = UdpSocket::bind(bind)?;
        socket.set_nonblocking(true)?;
        // Connecting a UDP socket filters out anything not from the server.
        socket.connect(server)?;
        Ok(NetClient {
            socket,
            endpoint: Endpoint::new(PROTOCOL_ID, 0.0),
            link: Link::Connecting,
            token,
            name,
            start: Instant::now(),
            last_request: f64::NEG_INFINITY,
            handshake_started: 0.0,
            warned_unanswered: false,
            server_addr_text: server.to_string(),
        })
    }

    pub fn now(&self) -> f64 {
        self.start.elapsed().as_secs_f64()
    }

    pub fn rtt_ms(&self) -> f32 {
        self.endpoint.rtt() * 1000.0
    }

    pub fn packet_loss(&self) -> f32 {
        self.endpoint.packet_loss()
    }

    fn send_raw(&self, bytes: &[u8]) {
        if let Err(e) = self.socket.send(bytes) {
            // The peer being unreachable surfaces here on Windows; it is not a
            // reason to tear anything down, since the server may come back.
            if e.kind() != ErrorKind::WouldBlock && e.kind() != ErrorKind::ConnectionReset {
                warn!("send failed: {e}");
            }
        }
    }

    fn request_connection(&mut self, now: f64) {
        let mut w = begin_packet(PROTOCOL_ID, PacketKind::ConnectionRequest);
        w.u64(self.token).string(&self.name);
        let bytes = w.into_inner();
        self.send_raw(&bytes);
        self.last_request = now;
    }

    /// Queues a reliable message.
    pub fn send_reliable(&mut self, message: &ClientMessage) {
        if self.link != Link::Connected {
            return;
        }
        if let Err(e) = self.endpoint.queue_reliable(message.to_vec()) {
            warn!("could not queue message: {e}");
        }
    }

    pub fn purchase(&mut self, powerup: PowerUp) {
        self.send_reliable(&ClientMessage::Purchase(powerup));
    }

    /// Asks for a sortie. Reliable for the same reason a purchase is: it
    /// happens once, and an input frame is never retransmitted.
    pub fn launch_plane(&mut self) {
        self.send_reliable(&ClientMessage::LaunchPlane);
    }

    /// Sends one packet carrying this tick's input.
    pub fn send_input(&mut self, frame: InputFrame) {
        if self.link != Link::Connected {
            return;
        }
        let now = self.now();
        let payload = ClientMessage::Input(frame).to_vec();
        let packet = self.endpoint.build_packet(now, Some(&payload));
        self.send_raw(&packet);
    }

    /// Asks the server to restart the match for everyone on a fresh map.
    /// Tells the server what the harvester should do when left alone.
    pub fn set_harvester_mode(&mut self, mode: HarvesterMode) {
        self.send_reliable(&ClientMessage::SetHarvesterMode(mode));
    }

    pub fn request_new_game(&mut self) {
        self.send_reliable(&ClientMessage::NewGame);
    }

    pub fn announce_leave(&mut self) {
        if self.link == Link::Connected {
            self.send_reliable(&ClientMessage::Leave);
            let now = self.now();
            let packet = self.endpoint.build_packet(now, None);
            self.send_raw(&packet);
        }
    }

    /// Waits briefly for the server's verdict on the handshake, before the
    /// window opens.
    ///
    /// Only a refusal is worth reporting here: it is the one answer that never
    /// changes, and the reason for it -- a name somebody else is already
    /// playing under, most of all -- belongs on the terminal the player
    /// launched from rather than buried in a note behind an empty field.
    ///
    /// Silence is not failure. A server that is slow, absent, or on an address
    /// nothing is listening to is left to the usual handshake loop, which
    /// retries and says so after a few seconds. An acceptance is simply thrown
    /// away: the loop asks again and the server, which already has the
    /// connection, answers again.
    pub fn await_verdict(&mut self, patience: Duration) -> Result<(), DenyReason> {
        // A blocking read with a short timeout, rather than spinning. The
        // socket goes back to non-blocking before anything else touches it.
        let _ = self.socket.set_nonblocking(false);
        let _ = self.socket.set_read_timeout(Some(Duration::from_millis(50)));

        let deadline = Instant::now() + patience;
        let mut verdict = Ok(());
        let mut buf = [0u8; MAX_PACKET];
        'wait: while Instant::now() < deadline {
            let now = self.now();
            if now - self.last_request >= HANDSHAKE_RETRY {
                self.request_connection(now);
            }
            while let Ok(len) = self.socket.recv(&mut buf) {
                let Ok((kind, mut reader)) = parse_packet(PROTOCOL_ID, &buf[..len]) else {
                    continue;
                };
                match kind {
                    PacketKind::ConnectionDenied => {
                        let reason = reader
                            .u8()
                            .ok()
                            .and_then(DenyReason::from_u8)
                            .unwrap_or(DenyReason::ServerFull);
                        self.link = Link::Denied(reason);
                        verdict = Err(reason);
                        break 'wait;
                    }
                    PacketKind::ConnectionAccepted => break 'wait,
                    _ => {}
                }
            }
        }

        let _ = self.socket.set_read_timeout(None);
        let _ = self.socket.set_nonblocking(true);
        verdict
    }

    fn reset_for_retry(&mut self) {
        let now = self.now();
        self.endpoint = Endpoint::new(PROTOCOL_ID, now);
        self.link = Link::Connecting;
        self.last_request = f64::NEG_INFINITY;
        self.handshake_started = now;
        self.warned_unanswered = false;
    }
}

/// Leaves the match and ends the process.
///
/// Telling the server on the way out frees the slot immediately instead of
/// making it wait for the connection to time out.
pub fn leave_and_exit(net: &mut NetClient) -> ! {
    net.announce_leave();
    std::process::exit(0);
}

/// Drains the socket and drives the handshake. Runs before anything reads state.
pub fn poll(mut net: ResMut<NetClient>, time: Res<Time>, mut state: ResMut<GameState>) {
    let now = net.now();
    // Snapshots are stamped on the clock `state::interpolate` compares
    // against, not on the connection's own. The two do not share an origin:
    // the socket opens before the window does, so `net.now()` runs ahead by
    // however long this client took to start. Mixing them made the world
    // render that startup time further into the past -- a different amount
    // on every client, and long enough to see a hit land before the shot
    // arrived.
    let render_now = time.elapsed_secs_f64();
    let mut buf = [0u8; MAX_PACKET];

    loop {
        let len = match net.socket.recv(&mut buf) {
            Ok(n) => n,
            Err(e) if e.kind() == ErrorKind::WouldBlock => break,
            // No listener at the far end yet; keep trying.
            Err(e) if e.kind() == ErrorKind::ConnectionReset => break,
            Err(e) => {
                warn!("recv failed: {e}");
                break;
            }
        };

        let (kind, mut reader) = match parse_packet(PROTOCOL_ID, &buf[..len]) {
            Ok(v) => v,
            // Stray or mismatched traffic; ignore it rather than dying.
            Err(_) => continue,
        };

        match kind {
            PacketKind::ConnectionAccepted => {
                let (Ok(player_id), Ok(seed), Ok(tick)) =
                    (reader.u8(), reader.u64(), reader.u32())
                else {
                    continue;
                };
                let _ = tick;
                if net.link != Link::Connected {
                    net.link = Link::Connected;
                    state.local_player = Some(player_id);
                    state.adopt_world(seed);
                    let name = state.name_of(player_id);
                    state.note(format!("Connected as {name}"));
                    info!("connected as player {player_id}");
                }
            }

            PacketKind::ConnectionDenied => {
                let reason =
                    reader.u8().ok().and_then(DenyReason::from_u8).unwrap_or(DenyReason::ServerFull);
                if net.link != Link::Denied(reason) {
                    state.note(format!("Rejected: {}", reason.describe()));
                    error!("connection denied: {}", reason.describe());
                }
                net.link = Link::Denied(reason);
            }

            PacketKind::Disconnect => {
                // The server has forgotten us; handshake again from scratch.
                if net.link == Link::Connected {
                    state.note("Connection reset; reconnecting");
                }
                net.reset_for_retry();
            }

            PacketKind::Payload => {
                let incoming = match net.endpoint.receive(now, &mut reader) {
                    Ok(v) => v,
                    Err(_) => continue,
                };
                for message in incoming.reliable {
                    if let Ok(msg) = ServerMessage::from_slice(&message) {
                        handle_message(&mut net, &mut state, render_now, msg);
                    }
                }
                if let Some(payload) = incoming.unreliable {
                    if let Ok(ServerMessage::Snapshot(snapshot)) =
                        ServerMessage::from_slice(&payload)
                    {
                        state.push_snapshot(render_now, snapshot);
                    }
                }
            }

            PacketKind::ConnectionRequest => {}
        }
    }

    match net.link {
        Link::Connecting => {
            if now - net.last_request >= HANDSHAKE_RETRY {
                net.request_connection(now);
            }
            if !net.warned_unanswered && now - net.handshake_started >= HANDSHAKE_UNANSWERED {
                net.warned_unanswered = true;
                warn!(
                    "no reply from {} after {HANDSHAKE_UNANSWERED:.0}s. Check the server is                      running, that the port is open, and that it is bound to the same address                      family (a server on 0.0.0.0 cannot hear an IPv6 client).",
                    net.server_addr_text
                );
                state.note("No reply from the server");
            }
        }
        Link::Connected => {
            if net.endpoint.time_since_recv(now) > TIMEOUT {
                state.note("Lost contact with the server; reconnecting");
                warn!("connection timed out; retrying");
                net.reset_for_retry();
            }
        }
        Link::Denied(_) => {}
    }
}

fn handle_message(
    net: &mut NetClient,
    state: &mut GameState,
    render_now: f64,
    message: ServerMessage,
) {
    match message {
        ServerMessage::Welcome { player_id, world_seed, .. } => {
            state.local_player = Some(player_id);
            state.adopt_world(world_seed);
            net.link = Link::Connected;
        }
        ServerMessage::Roster(roster) => state.roster = roster,
        ServerMessage::Snapshot(snapshot) => state.push_snapshot(render_now, snapshot),
        ServerMessage::Event(event) => {
            // A couple of events change the client's own world before they are
            // narrated; the rest are purely for the log.
            if let GameEvent::MatchReset { world_seed, .. } = event {
                state.begin_new_match(world_seed);
            }
            let text = describe_event(state, event);
            if let Some(text) = text {
                state.note(text);
            }
        }
    }
}

fn describe_event(state: &GameState, event: GameEvent) -> Option<String> {
    let me = state.local_player;
    let name = |id: u8| state.name_of(id);
    Some(match event {
        GameEvent::PlayerJoined { player } => format!("{} joined", name(player)),
        GameEvent::PlayerLeft { player } => format!("{} disconnected", name(player)),
        GameEvent::MatchStarted => "The match has begun".to_string(),
        GameEvent::PurchaseAccepted { powerup, credits } => {
            format!("Bought {} ({credits} ore left)", powerup.name())
        }
        GameEvent::PurchaseRejected { powerup, reason } => {
            format!("Cannot buy {}: {}", powerup.name(), reason.describe())
        }
        // A tank lost to a hillside is charged to its own driver, so `by` is
        // the victim. Saying somebody destroyed their own tank reads as a bug.
        GameEvent::TankDestroyed { player, by } if by == player => {
            if Some(player) == me {
                "You wrecked your own tank".to_string()
            } else {
                format!("{} wrecked their own tank", name(player))
            }
        }
        GameEvent::TankDestroyed { player, by } => {
            if Some(player) == me {
                format!("Your tank was destroyed by {}", name(by))
            } else {
                format!("{} destroyed {}'s tank", name(by), name(player))
            }
        }
        GameEvent::TankRespawned { player } if Some(player) == me => {
            "Your tank is back on the field".to_string()
        }
        GameEvent::TankRespawned { .. } => return None,
        GameEvent::HarvesterDisabled { player } => {
            if Some(player) == me {
                "Your harvester is disabled -- defend it!".to_string()
            } else {
                format!("{}'s harvester is disabled", name(player))
            }
        }
        GameEvent::HarvesterRescued { player } => {
            if Some(player) == me {
                "Your harvester is back online".to_string()
            } else {
                format!("{} rescued their harvester", name(player))
            }
        }
        GameEvent::SentinelDestroyed { player } => {
            if Some(player) == me {
                "Your base gun is down".to_string()
            } else {
                format!("{}'s base gun is down", name(player))
            }
        }
        GameEvent::SentinelRebuilt { player } if Some(player) == me => {
            "Your base gun is back".to_string()
        }
        GameEvent::SentinelRebuilt { .. } => return None,
        GameEvent::OreSeized { by, from, amount } => {
            if Some(by) == me {
                format!("Seized {amount} ore from {}", name(from))
            } else if Some(from) == me {
                format!("{} seized your {amount} ore", name(by))
            } else {
                format!("{} seized {amount} ore from {}", name(by), name(from))
            }
        }
        GameEvent::HarvesterCaptured { by, from } => {
            format!("{} captured {}'s harvester", name(by), name(from))
        }
        // A capture puts you off the field for a minute and then hands you a
        // fresh pair of vehicles and an empty bank -- unless it also emptied
        // the field, in which case the `GameOver` behind this event is the one
        // that counts and this note is overtaken a moment later.
        GameEvent::PlayerEliminated { player } => {
            if Some(player) == me {
                format!("Harvester lost -- back in {:.0}s", sim::CAPTURE_LOCKOUT)
            } else {
                format!("{} is down for a minute", name(player))
            }
        }
        GameEvent::PlayerReturned { player } => {
            if Some(player) == me {
                "You are back, with your upgrades and nothing else".to_string()
            } else {
                format!("{} is back on the field", name(player))
            }
        }
        GameEvent::GameOver { winner } => {
            if Some(winner) == me {
                "You win!".to_string()
            } else {
                format!("{} wins the match", name(winner))
            }
        }
        GameEvent::MatchReset { by, .. } => {
            if Some(by) == me {
                "You started a new match".to_string()
            } else {
                format!("{} started a new match", name(by))
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use orewar_shared::bytes::Writer;

    /// A server that answers one handshake, and nothing else.
    fn stand_in_server(reply: Option<DenyReason>) -> SocketAddr {
        let socket = UdpSocket::bind("127.0.0.1:0").expect("bind an ephemeral port");
        let addr = socket.local_addr().unwrap();
        std::thread::spawn(move || {
            let mut buf = [0u8; MAX_PACKET];
            let Ok((_, from)) = socket.recv_from(&mut buf) else { return };
            let w: Writer = match reply {
                Some(reason) => {
                    let mut w = begin_packet(PROTOCOL_ID, PacketKind::ConnectionDenied);
                    w.u8(reason as u8);
                    w
                }
                None => {
                    let mut w = begin_packet(PROTOCOL_ID, PacketKind::ConnectionAccepted);
                    w.u8(0).u64(1).u32(0);
                    w
                }
            };
            let _ = socket.send_to(w.as_slice(), from);
        });
        addr
    }

    fn client_to(addr: SocketAddr) -> NetClient {
        NetClient::connect(addr, 7, "Ash".to_string()).expect("open a socket")
    }

    /// The refusal has to arrive before the window does: it is the one answer
    /// that retrying cannot change.
    #[test]
    fn a_refusal_is_reported_at_once() {
        let addr = stand_in_server(Some(DenyReason::NameTaken));
        let mut net = client_to(addr);
        assert_eq!(net.await_verdict(Duration::from_secs(3)), Err(DenyReason::NameTaken));
    }

    /// Being let in is not worth waiting around for: the handshake loop asks
    /// again a quarter of a second later and the server, which already has the
    /// connection, answers again.
    #[test]
    fn an_acceptance_lets_the_game_start() {
        let addr = stand_in_server(None);
        let mut net = client_to(addr);
        assert_eq!(net.await_verdict(Duration::from_secs(3)), Ok(()));
    }

    /// Silence is not a refusal. A server that is slow, absent, or on an
    /// address nothing is listening to is left to the usual retry loop, which
    /// says so on its own after a few seconds -- opening the window and waiting
    /// is the right answer, not exiting.
    #[test]
    fn a_server_that_says_nothing_is_not_a_refusal() {
        // Nothing is bound here: on Windows this comes back as a connection
        // reset rather than as silence, which must not read as an answer.
        let addr: SocketAddr = "127.0.0.1:1".parse().unwrap();
        let mut net = client_to(addr);
        assert_eq!(net.await_verdict(Duration::from_millis(150)), Ok(()));
    }
}
