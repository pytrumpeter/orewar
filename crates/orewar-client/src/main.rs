//! Orewar client.
//!
//! ```text
//! orewar-client [--server HOST:PORT] [--name NAME] [--token N] [--token-file PATH]
//! ```
//!
//! The client renders and predicts; it never decides anything. Every outcome
//! comes from the server.

mod camera;
mod coords;
mod effects;
mod field;
mod miner_panel;
mod hud;
mod input;
mod menu;
mod net;
mod sentinels;
mod state;
mod vehicles;

use std::net::{SocketAddr, ToSocketAddrs};
use std::path::PathBuf;
use std::time::Duration;

use bevy::prelude::*;
use orewar_shared::protocol::DenyReason;
use orewar_shared::rng::Rng;
use orewar_shared::world::{DEFAULT_PORT, TICK_HZ};

use input::LocalInput;
use menu::MenuState;
use net::NetClient;
use state::GameState;

/// How long to wait for the server's answer to the handshake before opening the
/// window anyway.
///
/// Long enough for a server that is there to answer, short enough not to be a
/// pause anybody notices. One that is not there is not an error yet -- it may
/// still be starting -- so the game opens and keeps asking.
const HANDSHAKE_PATIENCE: Duration = Duration::from_millis(900);

struct Args {
    server: SocketAddr,
    name: String,
    token: u64,
}

fn print_help() {
    println!(
        "orewar-client [options]\n\n\
         --server HOST:PORT   server to join (default 127.0.0.1:{DEFAULT_PORT})\n\
         --name NAME          display name; also derives a stable identity\n\
         --token N            explicit identity token\n\
         --token-file PATH    file holding the identity token (default .orewar-token)\n\n\
         Identity decides which player slot you resume after a disconnect.\n\
         To run two clients on one machine, give each its own --name."
    );
}

/// Derives a stable token from a name, so `--name Ash` always returns to the
/// same player slot without needing a file on disk.
fn token_from_name(name: &str) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in name.as_bytes() {
        hash ^= *byte as u64;
        hash = hash.wrapping_mul(0x1000_0000_01b3);
    }
    // Run it through the mixer so similar names land far apart.
    Rng::new(hash).next_u64()
}

/// Reads the token file, creating it with a fresh random token if absent.
fn token_from_file(path: &PathBuf) -> u64 {
    if let Ok(text) = std::fs::read_to_string(path) {
        if let Ok(token) = text.trim().parse::<u64>() {
            return token;
        }
    }
    let token = Rng::seed_from_time();
    if let Err(e) = std::fs::write(path, token.to_string()) {
        warn!("could not save identity to {}: {e}", path.display());
    }
    token
}

/// Picks which resolved address to talk to, preferring IPv4.
///
/// `localhost` resolves to `::1` before `127.0.0.1` on Windows, and the server
/// binds `0.0.0.0` by default -- IPv4 only. Taking the resolver's first answer
/// therefore sent every packet to an IPv6 loopback nothing was listening on.
/// Nothing reports that: UDP has no connection to refuse, so the client simply
/// waits forever on a handshake that cannot arrive.
///
/// An explicit IPv6 address still works; this only decides ties.
fn prefer_ipv4(resolved: &[SocketAddr]) -> Option<SocketAddr> {
    resolved.iter().find(|a| a.is_ipv4()).or_else(|| resolved.first()).copied()
}

fn parse_args() -> Result<Args, String> {
    let mut server_text = format!("127.0.0.1:{DEFAULT_PORT}");
    let mut name = String::new();
    let mut explicit_token: Option<u64> = None;
    let mut token_file: Option<PathBuf> = None;

    let mut it = std::env::args().skip(1);
    while let Some(arg) = it.next() {
        let mut value = || it.next().ok_or_else(|| format!("{arg} needs a value"));
        match arg.as_str() {
            "--server" => server_text = value()?,
            "--name" => name = value()?,
            "--token" => {
                explicit_token =
                    Some(value()?.parse().map_err(|_| "--token must be a number".to_string())?)
            }
            "--token-file" => token_file = Some(PathBuf::from(value()?)),
            "-h" | "--help" => {
                print_help();
                std::process::exit(0);
            }
            other => return Err(format!("unrecognized argument: {other}")),
        }
    }

    // Accept a bare host and supply the default port.
    if !server_text.contains(':') {
        server_text = format!("{server_text}:{DEFAULT_PORT}");
    }
    let resolved: Vec<SocketAddr> = server_text
        .to_socket_addrs()
        .map_err(|e| format!("could not resolve {server_text}: {e}"))?
        .collect();
    let server =
        prefer_ipv4(&resolved).ok_or_else(|| format!("no address found for {server_text}"))?;

    // Precedence: an explicit token wins; then a name, which is the convenient
    // way to run several clients on one machine; then the token file.
    let token = match (explicit_token, token_file, name.is_empty()) {
        (Some(t), _, _) => t,
        (None, Some(path), _) => token_from_file(&path),
        (None, None, false) => token_from_name(&name),
        (None, None, true) => token_from_file(&PathBuf::from(".orewar-token")),
    };

    Ok(Args { server, name, token })
}

fn main() {
    let args = match parse_args() {
        Ok(a) => a,
        Err(e) => {
            eprintln!("error: {e}\n");
            print_help();
            std::process::exit(2);
        }
    };

    let mut client = match NetClient::connect(args.server, args.token, args.name.clone()) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("error: could not open a socket: {e}");
            std::process::exit(1);
        }
    };
    println!("Orewar: joining {} as {}", args.server, if args.name.is_empty() { "(unnamed)" } else { &args.name });

    // Ask before opening a window. A refusal never becomes an acceptance, and
    // the commonest one -- a name somebody is already playing under -- is a
    // mistake made on the command line and best answered there.
    if let Err(reason) = client.await_verdict(HANDSHAKE_PATIENCE) {
        eprintln!("error: {} refused the connection: {}", args.server, reason.describe());
        if reason == DenyReason::NameTaken {
            eprintln!("       A name is an identity here: it is what the server knows you by,");
            eprintln!("       and what returns you to your own ore and vehicles after a drop.");
            eprintln!("       Two clients cannot share one. Start this one with another --name.");
        }
        std::process::exit(1);
    }

    App::new()
        .add_plugins(DefaultPlugins.set(WindowPlugin {
            primary_window: Some(Window {
                title: "Orewar".into(),
                // Logical pixels, so on a HiDPI display the physical window is
                // larger by the scale factor. Keep the default modest enough to
                // fit a 1280x720 logical desktop; the window is resizable.
                resolution: (1180, 680).into(),
                position: WindowPosition::At(IVec2::new(30, 30)),
                ..default()
            }),
            ..default()
        }))
        .insert_resource(ClearColor(field::SKY_COLOR))
        // Prediction must advance in the same sized steps as the server's
        // simulation, or it drifts from authority every single tick.
        .insert_resource(Time::<Fixed>::from_hz(TICK_HZ as f64))
        .insert_resource(client)
        .init_resource::<GameState>()
        .init_resource::<LocalInput>()
        .init_resource::<MenuState>()
        .init_resource::<camera::Overview>()
        .add_systems(
            Startup,
            (
                field::setup,
                vehicles::setup,
                camera::setup,
                hud::setup,
                menu::setup,
                miner_panel::setup,
                sentinels::setup,
                effects::setup,
            ),
        )
        // After `vehicles::setup`, which is where the mesh it draws is built.
        .add_systems(Startup, vehicles::setup_bombsight.after(vehicles::setup))
        // Networking runs before anything reads state, so a frame always sees
        // the freshest snapshot that has arrived.
        .add_systems(PreUpdate, net::poll)
        .add_systems(FixedUpdate, input::send_input)
        .add_systems(
            Update,
            (
                // The menu takes Escape first and gates the controls, so it
                // has to settle before input is gathered.
                menu::toggle,
                // The overview settles first: while it is up the controls are
                // gated, and gathering before that would drive a vehicle for a
                // frame after the camera had already left it.
                camera::overview_controls,
                input::gather,
                menu::update,
                state::interpolate_system,
                (
                    vehicles::sync_vehicles,
                    vehicles::sync_projectiles,
                    vehicles::sync_bombsight,
                    field::sync_ore,
                    field::sync_hills,
                    sentinels::sync,
                    camera::follow,
                    hud::update_texts,
                    hud::update_bars,
                    hud::update_panels,
                    hud::update_radar,
                    miner_panel::update,
                ),
                // After the render view is rebuilt: a shield arc rides the
                // interpolated hull, not the one from last frame.
                effects::spawn,
                effects::animate,
                quit_on_request,
            )
                .chain(),
        )
        .run();
}

/// Leaves the match cleanly on Ctrl-Q, for players who never open the menu.
fn quit_on_request(keys: Res<ButtonInput<KeyCode>>, mut net: ResMut<NetClient>) {
    let ctrl = keys.pressed(KeyCode::ControlLeft) || keys.pressed(KeyCode::ControlRight);
    if ctrl && keys.just_pressed(KeyCode::KeyQ) {
        net::leave_and_exit(&mut net);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{Ipv4Addr, Ipv6Addr};

    fn v4(port: u16) -> SocketAddr {
        SocketAddr::from((Ipv4Addr::LOCALHOST, port))
    }
    fn v6(port: u16) -> SocketAddr {
        SocketAddr::from((Ipv6Addr::LOCALHOST, port))
    }

    /// The resolver lists `::1` first for `localhost` on Windows. The server
    /// binds IPv4 by default, so following that order sends every packet into
    /// a void that reports nothing back.
    #[test]
    fn ipv4_wins_when_the_resolver_lists_ipv6_first() {
        assert_eq!(prefer_ipv4(&[v6(45701), v4(45701)]), Some(v4(45701)));
    }

    #[test]
    fn ipv6_is_used_when_it_is_all_there_is() {
        assert_eq!(prefer_ipv4(&[v6(45701)]), Some(v6(45701)));
    }

    #[test]
    fn nothing_resolved_is_not_a_panic() {
        assert_eq!(prefer_ipv4(&[]), None);
    }

    #[test]
    fn a_name_maps_to_a_stable_identity() {
        assert_eq!(token_from_name("Ash"), token_from_name("Ash"));
        assert_ne!(token_from_name("Ash"), token_from_name("Bo"));
    }
}
