//! Shared foundation for the Orewar client and server.
//!
//! This crate has no dependencies. That is deliberate: the authoritative server
//! links only against this, so it builds and runs on a headless box in seconds
//! without dragging in a renderer.
//!
//! It owns everything both sides must agree on byte-for-byte:
//!
//! * [`bytes`] / [`protocol`] — the wire format.
//! * [`net`] — reliability over UDP.
//! * [`world`] / [`sim`] — world generation, tuning constants, and the vehicle
//!   step function. The client runs the same [`sim::step_vehicle`] the server
//!   does, at the same fixed rate, which is what makes prediction converge with
//!   authority instead of fighting it.

pub mod bytes;
pub mod math;
pub mod net;
pub mod protocol;
pub mod rng;
pub mod sim;
pub mod world;

pub use math::{Vec2, vec2};
