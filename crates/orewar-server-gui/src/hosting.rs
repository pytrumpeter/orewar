//! The match, running on its own thread.
//!
//! The server keeps its own clock: thirty ticks a second, whatever the window
//! is doing. A window is a poor clock -- it slows down when it loses focus and
//! stops altogether when it is minimised -- and a match whose simulation
//! stuttered because nobody was looking at it would be a worse game for the
//! four people who are.
//!
//! So the two share nothing but a mutex: the thread leaves a status snapshot
//! and its log lines in it, and picks up the two things the window can ask for.

use std::net::{IpAddr, SocketAddr, UdpSocket};
use std::sync::{Arc, Mutex, PoisonError};

use bevy::prelude::*;
use orewar_server::server::{Server, Status};

/// What the window and the match pass between them.
#[derive(Default)]
struct Shared {
    /// The newest snapshot, taken by the window each frame.
    status: Option<Status>,
    /// Log lines since the window last looked.
    log: Vec<String>,
    /// Asked for by the window, honoured on the next pump.
    stop: bool,
    new_match: bool,
}

#[derive(Resource)]
pub struct Hosting {
    shared: Arc<Mutex<Shared>>,
    pub seed: u64,
    /// The address to read out to players on other machines.
    pub join_at: String,
}

impl Hosting {
    /// Binds the port, generates the world, and puts the match on its own
    /// thread. A port already in use is the one failure worth a message: it
    /// usually means a server is running here already.
    pub fn start(bind: &str, port: u16, seed: u64) -> Result<Hosting, String> {
        let mut server = Server::bind(bind, port, seed)
            .map_err(|e| format!("Could not listen on {bind}:{port} -- {e}"))?;
        let addr = server.addr();
        let seed = server.seed();
        let shared = Arc::new(Mutex::new(Shared::default()));

        let thread_shared = Arc::clone(&shared);
        std::thread::Builder::new()
            .name("orewar-match".into())
            .spawn(move || {
                loop {
                    let sleep = server.pump();
                    let lines = server.drain_log();
                    let status = server.status();
                    let restart = {
                        // A window that has panicked has poisoned this, and the
                        // match is no reason to go down with it.
                        let mut shared =
                            thread_shared.lock().unwrap_or_else(PoisonError::into_inner);
                        if shared.stop {
                            break;
                        }
                        shared.log.extend(lines);
                        shared.status = Some(status);
                        std::mem::take(&mut shared.new_match)
                    };
                    if restart {
                        server.new_match();
                        continue;
                    }
                    std::thread::sleep(sleep);
                }
                server.goodbye();
            })
            .map_err(|e| format!("Could not start the match thread: {e}"))?;

        Ok(Hosting { shared, seed, join_at: join_at(addr) })
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Shared> {
        self.shared.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Takes the newest snapshot, if one has been left since the last look.
    pub fn take_status(&self) -> Option<Status> {
        self.lock().status.take()
    }

    pub fn take_log(&self) -> Vec<String> {
        std::mem::take(&mut self.lock().log)
    }

    pub fn ask_for_new_match(&self) {
        self.lock().new_match = true;
    }

    /// Asks the match to end. The thread says goodbye to every client on its
    /// way out, so nobody is left staring at a field that has stopped.
    pub fn ask_to_stop(&self) {
        self.lock().stop = true;
    }
}

/// What a player on another machine has to type.
///
/// A server bound to every interface knows its port but not which of its
/// addresses anybody can reach, so ask the routing table: connecting a UDP
/// socket sends nothing, it only picks the interface a packet would leave by.
/// The address it is pointed at is from the documentation range, which is
/// never anybody's real machine.
fn join_at(addr: SocketAddr) -> String {
    if !addr.ip().is_unspecified() {
        return addr.to_string();
    }
    match outward_address() {
        Some(ip) => format!("{ip}:{}", addr.port()),
        // No network at all: a local match is still a match.
        None => format!("127.0.0.1:{}", addr.port()),
    }
}

fn outward_address() -> Option<IpAddr> {
    let probe = UdpSocket::bind("0.0.0.0:0").ok()?;
    probe.connect("203.0.113.1:9").ok()?;
    probe.local_addr().ok().map(|a| a.ip())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::Ipv4Addr;

    /// A server told exactly which address to listen on has already answered
    /// the question, and must be quoted rather than second-guessed.
    #[test]
    fn an_explicit_address_is_the_one_shown() {
        let addr = SocketAddr::from(([192, 168, 1, 50], 45701));
        assert_eq!(join_at(addr), "192.168.1.50:45701");
    }

    /// `0.0.0.0` is every interface, which is not something anybody can type.
    #[test]
    fn listening_everywhere_becomes_an_address_somebody_can_use() {
        let addr = SocketAddr::from((Ipv4Addr::UNSPECIFIED, 45701));
        let shown = join_at(addr);
        assert!(shown.ends_with(":45701"), "{shown}");
        assert!(!shown.starts_with("0.0.0.0"), "{shown}");
    }
}
