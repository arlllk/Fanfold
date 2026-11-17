//! JMAP mail source, built on Stalwart's own [`jmap-client`](https://docs.rs/jmap-client).
//!
//! * [`JmapMailSource`] reads new mail with a persisted cursor (see its docs);
//! * [`PushWatcher`] wakes the poller the moment the server announces new mail;
//! * [`connect`] opens the session.
//!
//! One deployment requirement comes from the client: it calls the API at the address the
//! server *advertises* in its session document. That address must be reachable from where
//! this runs (see the README, "Mail").

pub mod convert;
pub mod push;
pub mod source;

use std::time::Duration;

use jmap_client::client::Client;
use secrecy::{ExposeSecret, SecretString};
use thiserror::Error;
use url::Url;

pub use push::{PushConfig, PushWatcher};
pub use source::{CURSOR_KEY, JmapMailSource, ResilientMailSource};

/// Where and as whom to connect.
pub struct JmapSettings {
    /// Origin the session is fetched from, e.g. `http://mail.example.svc:8080`. The server
    /// then tells the client where its API lives; see the crate docs.
    pub base_url: Url,
    pub username: String,
    pub password: SecretString,
    pub timeout: Duration,
}

impl std::fmt::Debug for JmapSettings {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("JmapSettings")
            .field("base_url", &self.base_url.as_str())
            .field("username", &self.username)
            .finish_non_exhaustive()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum JmapError {
    #[error("could not reach the JMAP server: {0}")]
    Transport(String),
    #[error("the JMAP server rejected the credentials")]
    Auth,
    #[error("the JMAP server reported an error: {0}")]
    Server(String),
    #[error("unexpected JMAP response: {0}")]
    Malformed(String),
    /// The saved mail cursor cannot be continued from; start over from a backfill.
    #[error("the saved mail cursor is unusable")]
    StateUnusable,
}

impl From<jmap_client::Error> for JmapError {
    fn from(e: jmap_client::Error) -> Self {
        use jmap_client::{Error, core::error::MethodErrorType};
        match e {
            Error::Method(m)
                if matches!(
                    m.error(),
                    MethodErrorType::CannotCalculateChanges | MethodErrorType::InvalidArguments
                ) =>
            {
                Self::StateUnusable
            }
            Error::Server(s) if s.starts_with("401") || s.starts_with("403") => Self::Auth,
            Error::Problem(p) if matches!(p.status, Some(401 | 403)) => Self::Auth,
            Error::Transport(t) => Self::Transport(t.without_url().to_string()),
            Error::Parse(p) => Self::Malformed(p.to_string()),
            other => Self::Server(other.to_string()),
        }
    }
}

/// Open a session. Pass the origin only: the client appends `/.well-known/jmap`.
pub async fn connect(settings: &JmapSettings, timeout: Duration) -> Result<Client, JmapError> {
    let host = settings.base_url.host_str().unwrap_or_default().to_owned();
    Client::new()
        .credentials((
            settings.username.as_str(),
            settings.password.expose_secret(),
        ))
        // The discovery URL redirects to the session document on the same host; the client
        // only follows redirects to hosts it is told to trust.
        .follow_redirects([host])
        .timeout(timeout)
        .connect(settings.base_url.as_str().trim_end_matches('/'))
        .await
        .map_err(JmapError::from)
}
