//! HTTP adapter: a server-rendered UI with live updates, plus ops endpoints.
//!
//! * [`router::ui_router`]: dashboard, history, job pages and their actions, and the
//!   `/events` stream. Behind the gateway's login; protected against CSRF here.
//! * [`router::ops_router`]: `/healthz`, `/readyz`, `/metrics` on a separate port.
//! * [`problem`]: every error is an [RFC 9457] problem document.
//!
//! [RFC 9457]: https://www.rfc-editor.org/rfc/rfc9457

pub mod format;
pub mod forms;
pub mod metrics;
pub mod problem;
pub mod router;
pub mod security;
pub mod views;

pub use router::{WebDeps, ops_router, ui_router};
