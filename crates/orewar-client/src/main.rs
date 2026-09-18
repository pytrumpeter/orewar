//! Orewar client, on a desktop.
//!
//! ```text
//! orewar-client [--server HOST:PORT] [--name NAME] [--token N] [--token-file PATH]
//! ```
//!
//! Started with nothing, it opens on the connect screen and asks who to join.
//! Anything given on the command line fills that screen in and presses the
//! button, so a scripted launch still goes straight to the field.
//!
//! Nothing but the command line lives here. The game itself is in the library,
//! so a platform with no command line can start exactly the same app.

use std::path::PathBuf;

use orewar_client::{Args, build_app};
use orewar_shared::world::DEFAULT_PORT;

fn print_help() {
    println!(
        "orewar-client [options]\n\n\
         --server HOST:PORT   server to join (default 127.0.0.1:{DEFAULT_PORT})\n\
         --name NAME          display name; also derives a stable identity\n\
         --token N            explicit identity token\n\
         --token-file PATH    file holding the identity token (default .orewar-token)\n\n\
         Only --server skips the connect screen, and only alongside a name.\n\
         Everything else fills the screen in and leaves it up to be checked:\n\
         an identity says who you are, never where to go, so it is never\n\
         enough on its own to send the client somewhere.\n\
         Identity decides which player slot you resume after a disconnect.\n\
         To run two clients on one machine, give each its own name."
    );
}

fn parse_args() -> Result<Args, String> {
    let mut args = Args { server: None, name: None, token: None, token_file: None };

    let mut it = std::env::args().skip(1);
    while let Some(arg) = it.next() {
        let mut value = || it.next().ok_or_else(|| format!("{arg} needs a value"));
        match arg.as_str() {
            "--server" => args.server = Some(value()?),
            "--name" => args.name = Some(value()?),
            "--token" => {
                args.token =
                    Some(value()?.parse().map_err(|_| "--token must be a number".to_string())?)
            }
            "--token-file" => args.token_file = Some(PathBuf::from(value()?)),
            "-h" | "--help" => {
                print_help();
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
            eprintln!("error: {e}\n");
            print_help();
            std::process::exit(2);
        }
    };

    build_app(args).run();
}
