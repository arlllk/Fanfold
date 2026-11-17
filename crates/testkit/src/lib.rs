//! Test support, shared by every crate's tests.
//!
//! * [`memory`] — deterministic in-memory implementations of each port, with
//!   failure injection, for fast scenario tests of the use cases.
//! * [`contracts`] — behavioural test suites for ports. Every real adapter runs the
//!   same suite as the in-memory fake, which is what keeps the fakes honest.

// This crate only exists to support tests, where panicking on a broken
// expectation is the point.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    // `Arc::clone(&x)` would block the `Arc<T>` -> `Arc<dyn Port>` coercion.
    clippy::clone_on_ref_ptr
)]

pub mod contracts;
pub mod memory;
