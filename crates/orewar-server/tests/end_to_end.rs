//! End-to-end tests against the real server binary over a real UDP socket.
//!
//! The unit tests cover the simulation and the reliability layer in isolation.
//! These cover the thing neither can: that a client and server built from this
//! code actually talk to each other -- handshake, snapshot flow, input applied
//! to the right player, and resuming a session after a drop.
//!
//! No graphics are involved, so this runs anywhere.

use std::io::{BufRead, BufReader};
use std::net::{SocketAddr, UdpSocket};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use orewar_shared::bytes::{Decode, Encode};
use orewar_shared::net::{Endpoint, Incoming, MAX_PACKET, PacketKind, begin_packet, parse_packet};
use orewar_shared::protocol::{
    ClientMessage, GameStatus, InputFrame, PROTOCOL_ID, ServerMessage, Snapshot, VehicleSlot,
};
use orewar_shared::world::{PowerUp, TICK_DT};

/// A server subprocess, killed when the test ends however it ends.
struct Server {
    child: Child,
    addr: SocketAddr,
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn start_server(seed: u64) -> Server {
    // Port 0 so concurrent tests never collide; the server prints what it got.
    let mut child = Command::new(env!("CARGO_BIN_EXE_orewar-server"))
        .args(["--bind", "127.0.0.1", "--port", "0", "--seed", &seed.to_string()])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("the server binary should start");

    let stdout = child.stdout.take().expect("stdout was piped");
    let mut reader = BufReader::new(stdout);
    let mut line = String::new();
    reader.read_line(&mut line).expect("the server should announce its address");
    let addr: SocketAddr = line
        .strip_prefix("listening ")
        .unwrap_or_else(|| panic!("unexpected first line from server: {line:?}"))
        .trim()
        .parse()
        .expect("a parseable socket address");

    // Keep draining stdout, or the server eventually blocks on a full pipe.
    std::thread::spawn(move || {
        let mut sink = String::new();
        while reader.read_line(&mut sink).unwrap_or(0) > 0 {
            sink.clear();
        }
    });

    Server { child, addr }
}

/// A minimal client: the same wire code the real client uses, without Bevy.
struct TestClient {
    socket: UdpSocket,
    endpoint: Endpoint,
    start: Instant,
    pub player_id: u8,
    pub world_seed: u64,
    pub tick: u32,
    pub latest: Option<Snapshot>,
}

impl TestClient {
    fn connect(server: SocketAddr, token: u64, name: &str) -> Self {
        let socket = UdpSocket::bind("127.0.0.1:0").expect("bind an ephemeral port");
        socket.connect(server).expect("connect to the server");
        socket.set_read_timeout(Some(Duration::from_millis(40))).unwrap();

        let deadline = Instant::now() + Duration::from_secs(10);
        let mut buf = [0u8; MAX_PACKET];
        loop {
            assert!(Instant::now() < deadline, "handshake timed out");

            let mut w = begin_packet(PROTOCOL_ID, PacketKind::ConnectionRequest);
            w.u64(token).string(name);
            socket.send(w.as_slice()).expect("send a connection request");

            let retry_at = Instant::now() + Duration::from_millis(250);
            while Instant::now() < retry_at {
                let Ok(n) = socket.recv(&mut buf) else { continue };
                let Ok((kind, mut r)) = parse_packet(PROTOCOL_ID, &buf[..n]) else { continue };
                match kind {
                    PacketKind::ConnectionAccepted => {
                        let player_id = r.u8().unwrap();
                        let world_seed = r.u64().unwrap();
                        // From here on, drain only what has already arrived. A
                        // blocking read with a timeout longer than the server's
                        // tick never sees a gap and so never returns.
                        socket.set_nonblocking(true).unwrap();
                        return TestClient {
                            socket,
                            endpoint: Endpoint::new(PROTOCOL_ID, 0.0),
                            start: Instant::now(),
                            player_id,
                            world_seed,
                            tick: 0,
                            latest: None,
                        };
                    }
                    PacketKind::ConnectionDenied => panic!("server denied the connection"),
                    _ => {}
                }
            }
        }
    }

    fn now(&self) -> f64 {
        self.start.elapsed().as_secs_f64()
    }

    fn queue(&mut self, message: &ClientMessage) {
        self.endpoint.queue_reliable(message.to_vec()).expect("message should fit a packet");
    }

    /// Sends one tick of input and absorbs whatever has arrived.
    fn pump(&mut self, input: InputFrame) -> Vec<ServerMessage> {
        self.tick = self.tick.wrapping_add(1);
        let frame = InputFrame { tick: self.tick, ..input };
        let payload = ClientMessage::Input(frame).to_vec();
        let now = self.now();
        let packet = self.endpoint.build_packet(now, Some(&payload));
        let _ = self.socket.send(&packet);
        self.drain()
    }

    fn drain(&mut self) -> Vec<ServerMessage> {
        let mut out = Vec::new();
        let mut buf = [0u8; MAX_PACKET];
        while let Ok(n) = self.socket.recv(&mut buf) {
            let Ok((kind, mut r)) = parse_packet(PROTOCOL_ID, &buf[..n]) else { continue };
            if kind != PacketKind::Payload {
                continue;
            }
            let Ok(Incoming { reliable, unreliable, .. }) = self.endpoint.receive(self.now(), &mut r)
            else {
                continue;
            };
            for bytes in reliable {
                if let Ok(m) = ServerMessage::from_slice(&bytes) {
                    out.push(m);
                }
            }
            if let Some(bytes) = unreliable {
                if let Ok(m) = ServerMessage::from_slice(&bytes) {
                    if let ServerMessage::Snapshot(s) = &m {
                        // Keep only the newest; snapshots can arrive reordered.
                        if self.latest.as_ref().is_none_or(|prev| s.tick > prev.tick) {
                            self.latest = Some(s.clone());
                        }
                    }
                    out.push(m);
                }
            }
        }
        out
    }

    /// Runs ticks until `done` is satisfied, or panics with `what`.
    fn run_until(
        &mut self,
        input: InputFrame,
        seconds: f64,
        what: &str,
        mut done: impl FnMut(&TestClient) -> bool,
    ) -> Vec<ServerMessage> {
        let deadline = Instant::now() + Duration::from_secs_f64(seconds);
        let mut collected = Vec::new();
        loop {
            collected.extend(self.pump(input));
            if done(self) {
                return collected;
            }
            assert!(Instant::now() < deadline, "timed out waiting for {what}");
            std::thread::sleep(Duration::from_secs_f32(TICK_DT));
        }
    }

    fn me(&self) -> Option<&orewar_shared::protocol::PlayerSnapshot> {
        self.latest.as_ref()?.players.iter().find(|p| p.id == self.player_id)
    }
}

fn idle(slot: VehicleSlot) -> InputFrame {
    InputFrame { controlling: slot, ..Default::default() }
}

#[test]
fn a_client_can_join_and_receives_the_world() {
    let server = start_server(4242);
    let mut client = TestClient::connect(server.addr, 1, "Ash");

    assert_eq!(client.player_id, 0, "the first player should get slot 0");
    assert_eq!(client.world_seed, 4242, "the world seed must reach the client");

    client.run_until(idle(VehicleSlot::Tank), 5.0, "the first snapshot", |c| c.latest.is_some());

    let me = client.me().expect("our own player should be in the snapshot");
    assert!(me.tank.is_some(), "we should start with a tank");
    assert!(me.harvester.is_some(), "we should start with a harvester");

    // One player alone is not a match.
    assert_eq!(client.latest.as_ref().unwrap().status, GameStatus::Waiting);
}

#[test]
fn a_match_starts_when_a_second_player_joins() {
    let server = start_server(7);
    let mut one = TestClient::connect(server.addr, 11, "One");
    one.run_until(idle(VehicleSlot::Tank), 5.0, "the first snapshot", |c| c.latest.is_some());
    assert_eq!(one.latest.as_ref().unwrap().status, GameStatus::Waiting);

    let mut two = TestClient::connect(server.addr, 22, "Two");
    assert_eq!(two.player_id, 1, "the second player should get slot 1");

    one.run_until(idle(VehicleSlot::Tank), 6.0, "the match to start", |c| {
        c.latest.as_ref().is_some_and(|s| s.status == GameStatus::Running)
    });

    let snapshot = one.latest.as_ref().unwrap();
    assert_eq!(snapshot.players.len(), 2, "both players should appear in snapshots");
    two.drain();
}

#[test]
fn driving_forward_moves_the_tank_the_way_it_faces() {
    let server = start_server(99);
    let mut client = TestClient::connect(server.addr, 5, "Driver");
    client.run_until(idle(VehicleSlot::Tank), 5.0, "the first snapshot", |c| c.latest.is_some());

    let before = client.me().unwrap().tank.expect("a tank");
    let heading = orewar_shared::math::Vec2::from_angle(before.yaw);

    let forward =
        InputFrame { controlling: VehicleSlot::Tank, throttle: 1.0, ..Default::default() };
    // Wait on distance covered, not on speed: a tank reaches 5 u/s in a fifth
    // of a second, having barely moved.
    let origin = before.pos;
    client.run_until(forward, 8.0, "the tank to cover ground", |c| {
        c.me().and_then(|p| p.tank).is_some_and(|t| t.pos.distance(origin) > 6.0)
    });

    let after = client.me().unwrap().tank.expect("a tank");
    let travelled = after.pos - before.pos;
    assert!(travelled.length() > 6.0, "the tank should have moved: {travelled:?}");
    assert!(
        travelled.normalize_or_zero().dot(heading) > 0.9,
        "the tank moved {travelled:?}, which is not along its heading {heading:?}"
    );
}

#[test]
fn an_unaffordable_purchase_is_rejected() {
    use orewar_shared::protocol::{GameEvent, RejectReason};

    let server = start_server(3);
    let mut client = TestClient::connect(server.addr, 8, "Buyer");
    client.run_until(idle(VehicleSlot::Tank), 5.0, "the first snapshot", |c| c.latest.is_some());
    client.queue(&ClientMessage::Purchase(PowerUp::Radar));

    let mut rejected = false;
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline && !rejected {
        for message in client.pump(idle(VehicleSlot::Tank)) {
            if let ServerMessage::Event(GameEvent::PurchaseRejected { powerup, reason }) = message {
                assert_eq!(powerup, PowerUp::Radar);
                assert_eq!(reason, RejectReason::NotEnoughCredits);
                rejected = true;
            }
        }
        std::thread::sleep(Duration::from_secs_f32(TICK_DT));
    }
    assert!(rejected, "the server should have refused an unaffordable purchase");
    assert!(
        !PowerUp::Radar.held(client.me().unwrap().powerups),
        "a refused purchase must not be granted"
    );
}

/// The point of identifying players by token rather than by address.
#[test]
fn reconnecting_with_the_same_token_resumes_the_same_player() {
    let server = start_server(1234);

    // Two players so the match is actually running.
    let mut one = TestClient::connect(server.addr, 111, "One");
    let mut two = TestClient::connect(server.addr, 222, "Two");
    one.run_until(idle(VehicleSlot::Tank), 6.0, "the match to start", |c| {
        c.latest.as_ref().is_some_and(|s| s.status == GameStatus::Running)
    });

    // Drive somewhere distinctive, then vanish.
    let forward =
        InputFrame { controlling: VehicleSlot::Tank, throttle: 1.0, ..Default::default() };
    one.run_until(forward, 6.0, "the tank to travel", |c| {
        c.me().and_then(|p| p.tank).is_some_and(|t| t.speed > 8.0)
    });
    let last_seen = one.me().unwrap().tank.expect("a tank").pos;
    let player_id = one.player_id;
    drop(one);

    // Come back on a brand new socket -- a different address entirely.
    let mut resumed = TestClient::connect(server.addr, 111, "One");
    assert_eq!(resumed.player_id, player_id, "the same token must resume the same slot");

    resumed.run_until(idle(VehicleSlot::Tank), 5.0, "a snapshot after resuming", |c| {
        c.latest.is_some()
    });
    let tank = resumed.me().expect("our player should still exist").tank.expect("still a tank");
    assert!(
        tank.pos.distance(last_seen) < 30.0,
        "the vehicle should still be where it was left: was {last_seen:?}, now {:?}",
        tank.pos
    );

    two.drain();
}

#[test]
fn a_fifth_player_is_turned_away() {
    let server = start_server(5);
    let mut clients: Vec<TestClient> =
        (0..4).map(|i| TestClient::connect(server.addr, 900 + i, "Player")).collect();
    for c in &mut clients {
        c.drain();
    }

    // The server is full, so the handshake gets a denial rather than an accept.
    let socket = UdpSocket::bind("127.0.0.1:0").unwrap();
    socket.connect(server.addr).unwrap();
    socket.set_read_timeout(Some(Duration::from_millis(200))).unwrap();

    let mut denied = false;
    let mut buf = [0u8; MAX_PACKET];
    for _ in 0..20 {
        let mut w = begin_packet(PROTOCOL_ID, PacketKind::ConnectionRequest);
        w.u64(12_345).string("Latecomer");
        socket.send(w.as_slice()).unwrap();
        if let Ok(n) = socket.recv(&mut buf) {
            if let Ok((PacketKind::ConnectionDenied, _)) = parse_packet(PROTOCOL_ID, &buf[..n]) {
                denied = true;
                break;
            }
        }
    }
    assert!(denied, "a fifth player should be refused");
}

