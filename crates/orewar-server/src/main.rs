//! Orewar dedicated server: a terminal and a loop.
//!
//! The match itself lives in [`orewar_server::server::Server`], which this
//! starts, pumps, and prints the log of. A host who would rather watch a window
//! than a terminal runs `orewar-server-gui` instead; it hosts exactly the same
//! match.
//!
//! Usage:
//!
//! ```text
//! orewar-server [--port 45701] [--bind 0.0.0.0] [--seed N]
//! ```

use std::time::{Duration, Instant};

use orewar_shared::rng::Rng;
use orewar_shared::world::{DEFAULT_PORT, MAX_PLAYERS, TICK_HZ};

use orewar_server::server::{Server, Status};

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

    let mut server = match Server::bind(&args.bind, args.port, args.seed) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("error: could not bind {}:{}: {e}", args.bind, args.port);
            std::process::exit(1);
        }
    };

    // The address it actually got, which differs from the one asked for when
    // port 0 was requested. Printed first and machine-readably so tooling can
    // find the port.
    println!("listening {}", server.addr());
    println!("Orewar server ready.");
    println!("  world seed : {}", server.seed());
    println!("  tick rate  : {TICK_HZ} Hz");
    println!("  capacity   : {MAX_PLAYERS} players");
    println!("Waiting for players. Ctrl-C to stop.");

    let mut last_report = Instant::now();
    loop {
        let sleep = server.pump();
        for line in server.drain_log() {
            println!("{line}");
        }
        if last_report.elapsed() >= Duration::from_secs(15) {
            last_report = Instant::now();
            println!("{}", report(&server.status()));
        }
        std::thread::sleep(sleep);
    }
}

/// The periodic line: where the match is up to, and who is in it.
fn report(status: &Status) -> String {
    let mut line = format!("tick {} | {:?}", status.tick, status.state);
    for player in &status.players {
        line.push_str(&format!(
            " | {}#{} ore {} cap {}{}",
            player.name,
            player.id,
            player.ore_mined,
            player.captures,
            match (player.connected, player.rtt_ms) {
                (true, Some(ms)) => format!(" {ms:.0}ms"),
                (true, None) => String::new(),
                (false, _) => " (offline)".to_string(),
            }
        ));
    }
    line
}
