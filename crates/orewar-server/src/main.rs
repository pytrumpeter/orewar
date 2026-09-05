//! Orewar dedicated server.
//!
//! One machine runs this and acts as the manager of the match: it owns the
//! world, steps the simulation at a fixed rate, and answers every client with
//! its own view of the result. Clients never talk to each other.
//!
//! Usage:
//!
//! ```text
//! orewar-server [--port 45701] [--bind 0.0.0.0] [--seed N]
//! ```

mod game;

use std::collections::HashMap;
use std::io::ErrorKind;
use std::net::{SocketAddr, UdpSocket};
use std::time::{Duration, Instant};

use orewar_shared::bytes::{Decode, Encode};
use orewar_shared::net::{
    Endpoint, MAX_PACKET, PacketKind, begin_packet, parse_packet,
};
use orewar_shared::protocol::{
    ClientMessage, DenyReason, ORE_FULL_SYNC_INTERVAL, PROTOCOL_ID, ServerMessage,
};
use orewar_shared::rng::Rng;
use orewar_shared::world::{DEFAULT_PORT, MAX_PLAYERS, TICK_DT, TICK_HZ};

use game::Game;

/// Drop a connection after this long without a packet. The player's state stays
/// on the server so they can resume where they left off.
const TIMEOUT: f64 = 8.0;

struct Connection {
    addr: SocketAddr,
    player_id: u8,
    endpoint: Endpoint,
    /// Set for a fresh or resumed connection, which needs every ore amount
    /// rather than the usual delta.
    needs_full_ore: bool,
}

struct Args {
    bind: String,
    port: u16,
    seed: u64,
}

fn parse_args() -> Result<Args, String> {
    let mut args = Args { bind: "0.0.0.0".into(), port: DEFAULT_PORT, seed: Rng::seed_from_time() };
    let mut it = std::env::args().skip(1);
    while let Some(arg) = it.next() {
        let mut value = || it.next().ok_or_else(|| format!("{arg} needs a value"));
        match arg.as_str() {
            "--bind" => args.bind = value()?,
            "--port" => {
                args.port = value()?.parse().map_err(|_| "--port must be a number".to_string())?
            }
            "--seed" => {
                args.seed = value()?.parse().map_err(|_| "--seed must be a number".to_string())?
            }
            "-h" | "--help" => {
                println!("orewar-server [--bind ADDR] [--port N] [--seed N]");
                std::process::exit(0);
            }
            other => return Err(format!("unrecognized argument: {other}")),
        }
    }
    Ok(args)
}

fn main() {
    let args = match parse_args() {
        Ok(a) => a,
        Err(e) => {
            eprintln!("error: {e}");
            std::process::exit(2);
        }
    };

    let requested = format!("{}:{}", args.bind, args.port);
    let socket = match UdpSocket::bind(&requested) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("error: could not bind {requested}: {e}");
            std::process::exit(1);
        }
    };
    socket.set_nonblocking(true).expect("socket should support non-blocking mode");
    // The actual address, which differs from the request when port 0 was asked
    // for. Printed first and machine-readably so tooling can find the port.
    let addr = socket.local_addr().map_or(requested, |a| a.to_string());
    println!("listening {addr}");

    let mut game = Game::new(args.seed);
    let mut connections: HashMap<SocketAddr, Connection> = HashMap::new();

    println!("Orewar server ready.");
    println!("  world seed : {}", args.seed);
    println!("  tick rate  : {TICK_HZ} Hz");
    println!("  capacity   : {MAX_PLAYERS} players");
    println!("Waiting for players. Ctrl-C to stop.");

    let start = Instant::now();
    let tick_duration = Duration::from_secs_f64(1.0 / TICK_HZ as f64);
    let mut next_tick = Instant::now();
    let mut buf = [0u8; MAX_PACKET];
    let mut last_report = Instant::now();

    loop {
        let now = start.elapsed().as_secs_f64();

        // ------------------------------------------------------------------
        // Drain everything the socket has for us.
        // ------------------------------------------------------------------
        loop {
            let (len, from) = match socket.recv_from(&mut buf) {
                Ok(v) => v,
                Err(e) if e.kind() == ErrorKind::WouldBlock => break,
                // A closed peer can surface as a connection error on Windows;
                // it says nothing about the socket's health, so keep serving.
                Err(e) if e.kind() == ErrorKind::ConnectionReset => continue,
                Err(e) => {
                    eprintln!("recv error: {e}");
                    break;
                }
            };

            if let Err(e) =
                handle_packet(&socket, &mut game, &mut connections, from, &buf[..len], now)
            {
                // Malformed traffic is expected on a public port; never fatal.
                eprintln!("dropping packet from {from}: {e}");
            }
        }

        // ------------------------------------------------------------------
        // Time out silent connections.
        // ------------------------------------------------------------------
        let stale: Vec<SocketAddr> = connections
            .iter()
            .filter(|(_, c)| c.endpoint.time_since_recv(now) > TIMEOUT)
            .map(|(addr, _)| *addr)
            .collect();
        for addr in stale {
            if let Some(conn) = connections.remove(&addr) {
                println!("player {} timed out ({addr}); state retained", conn.player_id);
                game.disconnect(conn.player_id);
            }
        }

        // ------------------------------------------------------------------
        // Step the simulation on a fixed schedule.
        // ------------------------------------------------------------------
        let mut stepped = false;
        while Instant::now() >= next_tick {
            game.step(TICK_DT);
            next_tick += tick_duration;
            stepped = true;

            // Guard against falling so far behind that we spiral trying to
            // catch up, e.g. after the process is suspended.
            if Instant::now() > next_tick + Duration::from_millis(500) {
                next_tick = Instant::now();
                break;
            }
        }

        if stepped {
            broadcast(&socket, &mut game, &mut connections, now);
        }

        if last_report.elapsed() >= Duration::from_secs(15) {
            last_report = Instant::now();
            report_status(&game, &connections);
        }

        // Sleep until the next tick rather than spinning.
        let sleep = next_tick.saturating_duration_since(Instant::now());
        std::thread::sleep(sleep.min(tick_duration));
    }
}

fn handle_packet(
    socket: &UdpSocket,
    game: &mut Game,
    connections: &mut HashMap<SocketAddr, Connection>,
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

            // Requests are retransmitted until the client sees a reply, so an
            // already-connected address just gets the same answer again.
            if let Some(conn) = connections.get(&from) {
                send_accept(socket, from, conn.player_id, game);
                return Ok(());
            }

            match game.join(token, &name) {
                Some(player_id) => {
                    // The same player reconnecting from a new address: retire
                    // the old connection rather than serving both.
                    connections.retain(|_, c| c.player_id != player_id);

                    let mut endpoint = Endpoint::new(PROTOCOL_ID, now);
                    queue(&mut endpoint, &ServerMessage::Welcome {
                        player_id,
                        world_seed: game.seed,
                        tick: game.tick,
                    });
                    queue(&mut endpoint, &ServerMessage::Roster(game.roster()));
                    connections.insert(from, Connection {
                        addr: from,
                        player_id,
                        endpoint,
                        needs_full_ore: true,
                    });
                    send_accept(socket, from, player_id, game);
                    println!(
                        "player {player_id} ({}) connected from {from}",
                        game.player(player_id).map_or("?", |p| p.name.as_str())
                    );

                    // Everyone else needs to learn the new name.
                    let roster = ServerMessage::Roster(game.roster());
                    for conn in connections.values_mut() {
                        queue(&mut conn.endpoint, &roster);
                    }
                }
                None => {
                    let mut w = begin_packet(PROTOCOL_ID, PacketKind::ConnectionDenied);
                    w.u8(DenyReason::ServerFull as u8);
                    let _ = socket.send_to(w.as_slice(), from);
                }
            }
        }

        PacketKind::Payload => {
            let Some(conn) = connections.get_mut(&from) else {
                // An unknown sender may be a client that outlived our record of
                // it; nudge it to handshake again rather than ignoring it.
                let mut w = begin_packet(PROTOCOL_ID, PacketKind::Disconnect);
                w.u8(0);
                let _ = socket.send_to(w.as_slice(), from);
                return Ok(());
            };
            let incoming = conn
                .endpoint
                .receive(now, &mut reader)
                .map_err(|e| format!("bad payload: {e}"))?;
            let player_id = conn.player_id;

            for message in incoming.reliable {
                match ClientMessage::from_slice(&message) {
                    Ok(ClientMessage::Purchase(p)) => game.purchase(player_id, p),
                    Ok(ClientMessage::SetHarvesterMode(m)) => {
                        game.set_harvester_mode(player_id, m)
                    }
                    Ok(ClientMessage::Leave) => {
                        println!("player {player_id} left");
                        game.disconnect(player_id);
                        connections.remove(&from);
                        return Ok(());
                    }
                    Ok(ClientMessage::Input(f)) => game.set_input(player_id, f),
                    Ok(ClientMessage::NewGame) => {
                        let seed = Rng::seed_from_time();
                        game.restart(seed, player_id);
                        // Every client is holding ore amounts for a map that no
                        // longer exists, and deltas cannot repair that.
                        for conn in connections.values_mut() {
                            conn.needs_full_ore = true;
                        }
                        println!("player {player_id} started a new match (seed {seed})");
                    }
                    Err(e) => return Err(format!("bad reliable message: {e}")),
                }
            }
            if let Some(payload) = incoming.unreliable {
                match ClientMessage::from_slice(&payload) {
                    Ok(ClientMessage::Input(f)) => game.set_input(player_id, f),
                    Ok(_) => {}
                    Err(e) => return Err(format!("bad input: {e}")),
                }
            }
        }

        PacketKind::Disconnect => {
            if let Some(conn) = connections.remove(&from) {
                println!("player {} disconnected", conn.player_id);
                game.disconnect(conn.player_id);
            }
        }

        // Server-only packet kinds; a client sending one is confused.
        PacketKind::ConnectionAccepted | PacketKind::ConnectionDenied => {}
    }

    Ok(())
}

fn send_accept(socket: &UdpSocket, to: SocketAddr, player_id: u8, game: &Game) {
    let mut w = begin_packet(PROTOCOL_ID, PacketKind::ConnectionAccepted);
    w.u8(player_id).u64(game.seed).u32(game.tick);
    let _ = socket.send_to(w.as_slice(), to);
}

fn queue(endpoint: &mut Endpoint, message: &ServerMessage) {
    let bytes = message.to_vec();
    if let Err(e) = endpoint.queue_reliable(bytes) {
        // Only possible if a message outgrows a packet, which would be a bug
        // in the protocol rather than a runtime condition.
        eprintln!("failed to queue reliable message: {e}");
    }
}

fn broadcast(
    socket: &UdpSocket,
    game: &mut Game,
    connections: &mut HashMap<SocketAddr, Connection>,
    now: f64,
) {
    // Events are match-wide, so every connection gets the same reliable stream.
    let events: Vec<ServerMessage> =
        game.events.drain(..).map(ServerMessage::Event).collect();
    let roster_changed = events.iter().any(|e| {
        matches!(
            e,
            ServerMessage::Event(orewar_shared::protocol::GameEvent::PlayerJoined { .. })
                | ServerMessage::Event(orewar_shared::protocol::GameEvent::PlayerLeft { .. })
        )
    });
    let roster = roster_changed.then(|| ServerMessage::Roster(game.roster()));

    let periodic_full_ore = game.tick % ORE_FULL_SYNC_INTERVAL == 0;

    for conn in connections.values_mut() {
        for event in &events {
            queue(&mut conn.endpoint, event);
        }
        if let Some(roster) = &roster {
            queue(&mut conn.endpoint, roster);
        }

        let full_ore = periodic_full_ore || conn.needs_full_ore;
        conn.needs_full_ore = false;

        // Snapshots are per-viewer: each client gets the projectiles nearest to
        // its own vehicle and its own input acknowledgement.
        let snapshot = ServerMessage::Snapshot(game.snapshot_for(conn.player_id, full_ore));
        let payload = snapshot.to_vec();
        let packet = conn.endpoint.build_packet(now, Some(&payload));
        if let Err(e) = socket.send_to(&packet, conn.addr) {
            if e.kind() != ErrorKind::WouldBlock && e.kind() != ErrorKind::ConnectionReset {
                eprintln!("send to {} failed: {e}", conn.addr);
            }
        }
    }

    game.clear_dirty_ore();
}

fn report_status(game: &Game, connections: &HashMap<SocketAddr, Connection>) {
    let mut line = format!("tick {} | {:?}", game.tick, game.status);
    for player in game.players.iter().flatten() {
        let rtt = connections
            .values()
            .find(|c| c.player_id == player.id)
            .map(|c| c.endpoint.rtt() * 1000.0);
        line.push_str(&format!(
            " | {}#{} ore {} cap {}{}",
            player.name,
            player.id,
            player.ore_mined,
            player.captures,
            match (player.connected, rtt) {
                (true, Some(ms)) => format!(" {ms:.0}ms"),
                (true, None) => String::new(),
                (false, _) => " (offline)".to_string(),
            }
        ));
    }
    println!("{line}");
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The reliable stream must carry a `Welcome` that fits one packet, which is
    /// why it holds a world seed rather than the generated deposit list.
    #[test]
    fn the_handshake_messages_fit_a_single_packet() {
        use orewar_shared::net::MAX_MESSAGE;
        use orewar_shared::protocol::PlayerInfo;

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
}
