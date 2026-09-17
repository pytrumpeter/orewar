//! The socket, the connections, and the clock that steps them.
//!
//! One machine runs this and acts as the manager of the match: it owns the
//! world, steps the simulation at a fixed rate, and answers every client with
//! its own view of the result. Clients never talk to each other.
//!
//! Nothing here prints. What used to go straight to the terminal is pushed onto
//! a log the host drains, because a window cannot read stdout -- and because a
//! line worth printing is a line worth showing.

use std::collections::HashMap;
use std::collections::VecDeque;
use std::io::ErrorKind;
use std::net::{SocketAddr, UdpSocket};
use std::time::{Duration, Instant};

use orewar_shared::bytes::{Decode, Encode};
use orewar_shared::net::{Endpoint, MAX_PACKET, PacketKind, begin_packet, parse_packet};
use orewar_shared::protocol::{
    ClientMessage, DenyReason, GameEvent, GameStatus, HOST, ORE_FULL_SYNC_INTERVAL, PROTOCOL_ID,
    ServerMessage,
};
use orewar_shared::rng::Rng;
use orewar_shared::world::{TICK_DT, TICK_HZ};

use crate::game::Game;

/// Drop a connection after this long without a packet. The player's state stays
/// on the server so they can resume where they left off.
const TIMEOUT: f64 = 8.0;

/// How recently a connection must have been heard from for a second client on
/// the same identity to count as a duplicate rather than a reconnect.
///
/// A client that is playing sends input 30 times a second, so fifteen frames of
/// silence is already far more than a bad link produces. Kept short on purpose:
/// admitting a duplicate costs somebody a confusing match, but refusing a
/// genuine reconnect costs them their slot, and nobody restarts a game they
/// crashed out of in half a second.
const DUPLICATE_WINDOW: f64 = 0.5;

/// How many log lines are kept for a host that is not draining them.
///
/// The terminal drains every pump and the window every frame, so this only
/// matters while a window is minimised or a machine is asleep. Old lines go
/// first: the recent ones are the ones anybody wants.
const LOG_KEPT: usize = 200;

struct Connection {
    addr: SocketAddr,
    player_id: u8,
    endpoint: Endpoint,
    /// Set for a fresh or resumed connection, which needs every ore amount
    /// rather than the usual delta.
    needs_full_ore: bool,
}

/// What one player looks like from outside, for a status line or a table.
pub struct PlayerStatus {
    pub id: u8,
    pub name: String,
    pub ore_mined: u32,
    pub captures: u8,
    pub connected: bool,
    /// Round trip time in milliseconds, for a player currently connected.
    pub rtt_ms: Option<f32>,
}

/// A snapshot of the match, for whoever is hosting it to show.
pub struct Status {
    pub addr: SocketAddr,
    pub seed: u64,
    pub tick: u32,
    pub state: GameStatus,
    pub players: Vec<PlayerStatus>,
}

pub struct Server {
    socket: UdpSocket,
    addr: SocketAddr,
    game: Game,
    connections: HashMap<SocketAddr, Connection>,
    start: Instant,
    next_tick: Instant,
    tick_duration: Duration,
    log: VecDeque<String>,
}

impl Server {
    /// Binds the port and generates the world. The address it reports back is
    /// the one it actually got, which differs from the one asked for when port
    /// 0 was requested.
    pub fn bind(bind: &str, port: u16, seed: u64) -> std::io::Result<Server> {
        let socket = UdpSocket::bind(format!("{bind}:{port}"))?;
        socket.set_nonblocking(true)?;
        let addr = socket.local_addr()?;
        Ok(Server {
            socket,
            addr,
            game: Game::new(seed),
            connections: HashMap::new(),
            start: Instant::now(),
            next_tick: Instant::now(),
            tick_duration: Duration::from_secs_f64(1.0 / TICK_HZ as f64),
            log: VecDeque::new(),
        })
    }

    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    pub fn seed(&self) -> u64 {
        self.game.seed
    }

    /// Serves whatever is waiting and steps the simulation for however many
    /// ticks are due, then says how long there is until the next one.
    ///
    /// The caller sleeps for that long. Called more often than that it does
    /// nothing but read the socket, which is harmless -- a window running at
    /// its own refresh rate is exactly that case.
    pub fn pump(&mut self) -> Duration {
        let now = self.start.elapsed().as_secs_f64();

        // ------------------------------------------------------------------
        // Drain everything the socket has for us.
        // ------------------------------------------------------------------
        let mut buf = [0u8; MAX_PACKET];
        loop {
            let (len, from) = match self.socket.recv_from(&mut buf) {
                Ok(v) => v,
                Err(e) if e.kind() == ErrorKind::WouldBlock => break,
                // A closed peer can surface as a connection error on Windows;
                // it says nothing about the socket's health, so keep serving.
                Err(e) if e.kind() == ErrorKind::ConnectionReset => continue,
                Err(e) => {
                    self.note(format!("recv error: {e}"));
                    break;
                }
            };

            let packet = buf[..len].to_vec();
            if let Err(e) = self.handle_packet(from, &packet, now) {
                // Malformed traffic is expected on a public port; never fatal.
                self.note(format!("dropping packet from {from}: {e}"));
            }
        }

        // ------------------------------------------------------------------
        // Time out silent connections.
        // ------------------------------------------------------------------
        let stale: Vec<SocketAddr> = self
            .connections
            .iter()
            .filter(|(_, c)| c.endpoint.time_since_recv(now) > TIMEOUT)
            .map(|(addr, _)| *addr)
            .collect();
        for addr in stale {
            if let Some(conn) = self.connections.remove(&addr) {
                self.note(format!("player {} timed out ({addr}); state retained", conn.player_id));
                self.game.disconnect(conn.player_id);
            }
        }

        // ------------------------------------------------------------------
        // Step the simulation on a fixed schedule.
        // ------------------------------------------------------------------
        let mut stepped = false;
        while Instant::now() >= self.next_tick {
            self.game.step(TICK_DT);
            self.next_tick += self.tick_duration;
            stepped = true;

            // Guard against falling so far behind that we spiral trying to
            // catch up, e.g. after the process is suspended.
            if Instant::now() > self.next_tick + Duration::from_millis(500) {
                self.next_tick = Instant::now();
                break;
            }
        }

        if stepped {
            self.broadcast(now);
        }

        // Never longer than one tick: a client's packet should not wait for the
        // simulation before it is even read.
        self.next_tick.saturating_duration_since(Instant::now()).min(self.tick_duration)
    }

    /// Restarts the match on a fresh map, the way a player asking for one does.
    pub fn new_match(&mut self) {
        let seed = Rng::seed_from_time();
        self.game.restart(seed, HOST);
        for conn in self.connections.values_mut() {
            conn.needs_full_ore = true;
        }
        self.note(format!("the host started a new match (seed {seed})"));
    }

    /// Tells every client the match is over.
    ///
    /// Without it they sit through the eight-second timeout and then handshake
    /// into the dark, which looks like a network fault rather than a host who
    /// shut the game down.
    pub fn goodbye(&mut self) {
        let mut w = begin_packet(PROTOCOL_ID, PacketKind::Disconnect);
        w.u8(0);
        for conn in self.connections.values() {
            let _ = self.socket.send_to(w.as_slice(), conn.addr);
        }
        self.connections.clear();
    }

    /// Everything worth showing about the match right now.
    pub fn status(&self) -> Status {
        Status {
            addr: self.addr,
            seed: self.game.seed,
            tick: self.game.tick,
            state: self.game.status,
            players: self
                .game
                .players
                .iter()
                .flatten()
                .map(|p| PlayerStatus {
                    id: p.id,
                    name: p.name.clone(),
                    ore_mined: p.ore_mined,
                    captures: p.captures,
                    connected: p.connected,
                    rtt_ms: self
                        .connections
                        .values()
                        .find(|c| c.player_id == p.id)
                        .map(|c| c.endpoint.rtt() * 1000.0),
                })
                .collect(),
        }
    }

    /// Takes the log lines written since the last call.
    pub fn drain_log(&mut self) -> Vec<String> {
        self.log.drain(..).collect()
    }

    fn note(&mut self, line: String) {
        if self.log.len() >= LOG_KEPT {
            self.log.pop_front();
        }
        self.log.push_back(line);
    }

    /// Turns a client away, with the reason it can show its player.
    fn deny(&self, to: SocketAddr, reason: DenyReason) {
        let mut w = begin_packet(PROTOCOL_ID, PacketKind::ConnectionDenied);
        w.u8(reason as u8);
        let _ = self.socket.send_to(w.as_slice(), to);
    }

    fn send_accept(&self, to: SocketAddr, player_id: u8) {
        let mut w = begin_packet(PROTOCOL_ID, PacketKind::ConnectionAccepted);
        w.u8(player_id).u64(self.game.seed).u32(self.game.tick);
        let _ = self.socket.send_to(w.as_slice(), to);
    }

    fn handle_packet(
        &mut self,
        from: SocketAddr,
        packet: &[u8],
        now: f64,
    ) -> Result<(), String> {
        let (kind, mut reader) =
            parse_packet(PROTOCOL_ID, packet).map_err(|e| format!("bad header: {e}"))?;

        match kind {
            PacketKind::ConnectionRequest => {
                let token = reader.u64().map_err(|e| e.to_string())?;
                let name = reader.string().map_err(|e| e.to_string())?;

                // Requests are retransmitted until the client sees a reply, so
                // an already-connected address just gets the same answer again.
                if let Some(conn) = self.connections.get(&from) {
                    let player_id = conn.player_id;
                    self.send_accept(from, player_id);
                    return Ok(());
                }

                // A second client on the same identity is the duplicate-name
                // case that `Game::join` cannot see: the client hashes the name
                // into its token, so two players called `Ash` arrive as one.
                // Resuming would retire the first connection, whose client then
                // handshakes again and retires the second, forever.
                //
                // Liveness decides it rather than the `connected` flag, which
                // stays set for the whole timeout: a client that crashed goes
                // quiet immediately, so restarting it is still allowed, while a
                // client that is actually playing is sending input 30 times a
                // second and is never this quiet.
                let live_duplicate = self
                    .game
                    .player_by_token(token)
                    .map(|p| p.id)
                    .and_then(|id| self.connections.values().find(|c| c.player_id == id))
                    .is_some_and(|c| c.endpoint.time_since_recv(now) < DUPLICATE_WINDOW);
                if live_duplicate {
                    self.deny(from, DenyReason::NameTaken);
                    let who =
                        self.game.player_by_token(token).map_or("?", |p| p.name.as_str()).to_owned();
                    self.note(format!(
                        "refused {from}: {who} is already playing from another window"
                    ));
                    return Ok(());
                }

                match self.game.join(token, &name) {
                    Ok(player_id) => {
                        // The same player reconnecting from a new address:
                        // retire the old connection rather than serving both.
                        self.connections.retain(|_, c| c.player_id != player_id);

                        let mut endpoint = Endpoint::new(PROTOCOL_ID, now);
                        queue(&mut endpoint, &ServerMessage::Welcome {
                            player_id,
                            world_seed: self.game.seed,
                            tick: self.game.tick,
                        });
                        queue(&mut endpoint, &ServerMessage::Roster(self.game.roster()));
                        self.connections.insert(from, Connection {
                            addr: from,
                            player_id,
                            endpoint,
                            needs_full_ore: true,
                        });
                        self.send_accept(from, player_id);
                        let who =
                            self.game.player(player_id).map_or("?", |p| p.name.as_str()).to_owned();
                        self.note(format!("player {player_id} ({who}) connected from {from}"));

                        // Everyone else needs to learn the new name.
                        let roster = ServerMessage::Roster(self.game.roster());
                        for conn in self.connections.values_mut() {
                            queue(&mut conn.endpoint, &roster);
                        }
                    }
                    Err(reason) => {
                        self.deny(from, reason);
                        self.note(format!("refused {from} ({name:?}): {}", reason.describe()));
                    }
                }
            }

            PacketKind::Payload => {
                let Some(conn) = self.connections.get_mut(&from) else {
                    // An unknown sender may be a client that outlived our record
                    // of it; nudge it to handshake again rather than ignoring it.
                    let mut w = begin_packet(PROTOCOL_ID, PacketKind::Disconnect);
                    w.u8(0);
                    let _ = self.socket.send_to(w.as_slice(), from);
                    return Ok(());
                };
                let incoming = conn
                    .endpoint
                    .receive(now, &mut reader)
                    .map_err(|e| format!("bad payload: {e}"))?;
                let player_id = conn.player_id;

                for message in incoming.reliable {
                    match ClientMessage::from_slice(&message) {
                        Ok(ClientMessage::Purchase(p)) => self.game.purchase(player_id, p),
                        Ok(ClientMessage::LaunchPlane) => self.game.launch_plane(player_id),
                        Ok(ClientMessage::ToggleCheats) => self.game.toggle_cheats(),
                        Ok(ClientMessage::SetMinerMode(m)) => {
                            self.game.set_miner_mode(player_id, m)
                        }
                        Ok(ClientMessage::Leave) => {
                            self.note(format!("player {player_id} left"));
                            self.game.disconnect(player_id);
                            self.connections.remove(&from);
                            return Ok(());
                        }
                        Ok(ClientMessage::Input(f)) => self.game.set_input(player_id, f),
                        Ok(ClientMessage::NewGame) => {
                            let seed = Rng::seed_from_time();
                            self.game.restart(seed, player_id);
                            // Every client is holding ore amounts for a map that
                            // no longer exists, and deltas cannot repair that.
                            for conn in self.connections.values_mut() {
                                conn.needs_full_ore = true;
                            }
                            self.note(format!(
                                "player {player_id} started a new match (seed {seed})"
                            ));
                        }
                        Err(e) => return Err(format!("bad reliable message: {e}")),
                    }
                }
                if let Some(payload) = incoming.unreliable {
                    match ClientMessage::from_slice(&payload) {
                        Ok(ClientMessage::Input(f)) => self.game.set_input(player_id, f),
                        Ok(_) => {}
                        Err(e) => return Err(format!("bad input: {e}")),
                    }
                }
            }

            PacketKind::Disconnect => {
                if let Some(conn) = self.connections.remove(&from) {
                    self.note(format!("player {} disconnected", conn.player_id));
                    self.game.disconnect(conn.player_id);
                }
            }

            // Server-only packet kinds; a client sending one is confused.
            PacketKind::ConnectionAccepted | PacketKind::ConnectionDenied => {}
        }

        Ok(())
    }

    fn broadcast(&mut self, now: f64) {
        // Events are match-wide, so every connection gets the same reliable
        // stream.
        let events: Vec<ServerMessage> =
            self.game.events.drain(..).map(ServerMessage::Event).collect();
        let roster_changed = events.iter().any(|e| {
            matches!(
                e,
                ServerMessage::Event(GameEvent::PlayerJoined { .. })
                    | ServerMessage::Event(GameEvent::PlayerLeft { .. })
            )
        });
        let roster = roster_changed.then(|| ServerMessage::Roster(self.game.roster()));

        let periodic_full_ore = self.game.tick % ORE_FULL_SYNC_INTERVAL == 0;
        let mut failures = Vec::new();

        for conn in self.connections.values_mut() {
            for event in &events {
                queue(&mut conn.endpoint, event);
            }
            if let Some(roster) = &roster {
                queue(&mut conn.endpoint, roster);
            }

            let full_ore = periodic_full_ore || conn.needs_full_ore;
            conn.needs_full_ore = false;

            // Snapshots are per-viewer: each client gets the projectiles nearest
            // to its own vehicle and its own input acknowledgement.
            let snapshot =
                ServerMessage::Snapshot(self.game.snapshot_for(conn.player_id, full_ore));
            let payload = snapshot.to_vec();
            let packet = conn.endpoint.build_packet(now, Some(&payload));
            if let Err(e) = self.socket.send_to(&packet, conn.addr) {
                if e.kind() != ErrorKind::WouldBlock && e.kind() != ErrorKind::ConnectionReset {
                    failures.push(format!("send to {} failed: {e}", conn.addr));
                }
            }
        }

        self.game.clear_dirty_ore();
        for failure in failures {
            self.note(failure);
        }
    }
}

fn queue(endpoint: &mut Endpoint, message: &ServerMessage) {
    let bytes = message.to_vec();
    if let Err(e) = endpoint.queue_reliable(bytes) {
        // Only possible if a message outgrows a packet, which would be a bug
        // in the protocol rather than a runtime condition.
        eprintln!("failed to queue reliable message: {e}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use orewar_shared::net::MAX_MESSAGE;
    use orewar_shared::protocol::PlayerInfo;
    use orewar_shared::world::MAX_PLAYERS;

    /// The reliable stream must carry a `Welcome` that fits one packet, which is
    /// why it holds a world seed rather than the generated deposit list.
    #[test]
    fn the_handshake_messages_fit_a_single_packet() {
        let welcome =
            ServerMessage::Welcome { player_id: 3, world_seed: u64::MAX, tick: u32::MAX }.to_vec();
        assert!(welcome.len() <= MAX_MESSAGE, "welcome is {} bytes", welcome.len());

        let roster = ServerMessage::Roster(
            (0..MAX_PLAYERS as u8)
                .map(|id| PlayerInfo { id, name: "W".repeat(255), connected: true })
                .collect(),
        )
        .to_vec();
        assert!(roster.len() <= MAX_MESSAGE, "roster is {} bytes", roster.len());
    }

    /// Port 0 means "any free port", and a host that cannot say which one it
    /// got has nothing to tell its players.
    #[test]
    fn binding_reports_the_port_it_actually_got() {
        let server = Server::bind("127.0.0.1", 0, 1).expect("bind an ephemeral port");
        assert_ne!(server.addr().port(), 0);
    }

    /// The log is what a host shows instead of reading stdout, so it has to
    /// survive being left undrained -- and hand back what it kept.
    #[test]
    fn the_log_keeps_the_recent_lines_and_drains_once() {
        let mut server = Server::bind("127.0.0.1", 0, 1).expect("bind an ephemeral port");
        for i in 0..LOG_KEPT + 50 {
            server.note(format!("line {i}"));
        }
        let lines = server.drain_log();
        assert_eq!(lines.len(), LOG_KEPT);
        assert_eq!(lines.last().unwrap(), &format!("line {}", LOG_KEPT + 49));
        assert!(server.drain_log().is_empty());
    }

    /// Pumping an idle server must not ask its host to sleep longer than a
    /// tick, or a packet waits on the simulation before it is even read.
    #[test]
    fn a_pump_never_asks_for_more_than_one_tick() {
        let mut server = Server::bind("127.0.0.1", 0, 1).expect("bind an ephemeral port");
        let sleep = server.pump();
        assert!(sleep <= Duration::from_secs_f64(1.0 / TICK_HZ as f64));
    }
}
