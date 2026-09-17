//! The authoritative Orewar server.
//!
//! [`game`] owns the rules and the world; [`server`] owns the socket, the
//! connections, and the clock that steps them. Both are a library so that more
//! than one program can host a match: the dedicated `orewar-server` binary,
//! which is a terminal and a loop, and `orewar-server-gui`, which is the same
//! server with a window in front of it.

pub mod game;
pub mod server;
