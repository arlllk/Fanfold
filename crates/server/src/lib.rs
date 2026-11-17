//! Composition root.
//!
//! Everything that touches the outside world is chosen here: the declarative
//! configuration, where secrets come from, which adapters implement which port.
//! The logic itself lives in `fanfold-app` and `fanfold-domain`.

pub mod cli;
pub mod config;
pub mod infra;
pub mod ini;
pub mod runtime;
pub mod secrets;
