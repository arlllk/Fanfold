//! Application layer of the hexagon.
//!
//! * [`ports`] — the traits the application needs from the outside world
//!   (mail, storage, downloading, notification, time, randomness).
//! * [`ingest`] and [`worker`] — use cases, written only against ports.
//!
//! Adapters implement the ports; the composition root wires them together. Nothing
//! in this crate knows about Postgres, JMAP, Python or Discord.

pub mod admin;
pub mod error;
pub mod ingest;
pub mod library;
pub mod ports;
pub mod stats;
pub mod worker;
