//! Pure domain model.
//!
//! Nothing here performs IO, reads a clock, or draws randomness: time and jitter
//! are always passed in. That keeps every state machine deterministic, so it can be
//! property-tested, fuzzed and mutation-tested exhaustively.

pub mod breaker;
pub mod failure;
pub mod job;
pub mod library;
pub mod mail;
pub mod metadata;
pub mod retry;
pub mod session;
pub mod timestamp;
pub mod work;

pub use timestamp::Timestamp;
