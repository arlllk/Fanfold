//! JMAP push: wake the poller as soon as mail arrives.
//!
//! The server offers a Server-Sent Events stream (`eventSourceUrl`) announcing state
//! changes; `jmap-client` parses it. We use it purely as a *wake-up*: on an `Email` state
//! change the poll loop runs at once instead of waiting out its interval. Correctness
//! never depends on it: the poll interval is the safety net, and every (re)connect also
//! triggers a poll, so nothing announced while we were disconnected is missed.
//!
//! `jmap-client` puts no timeout on an open stream (only on connecting) and hides the
//! server's pings, so a connection that died silently would never be noticed. We therefore
//! cap how long one stream may live and reconnect (which also polls) when it expires.

use std::{sync::Arc, time::Duration};

use futures_util::StreamExt;
use jmap_client::{DataType, event_source::PushNotification};
use tokio::sync::{Notify, watch};

use crate::{JmapError, JmapSettings, connect};

#[derive(Debug, Clone, Copy)]
pub struct PushConfig {
    pub reconnect_min: Duration,
    pub reconnect_max: Duration,
    /// The longest one stream is kept open; it is then re-established. This bounds how long
    /// a silently dead connection can go unnoticed.
    pub max_stream_age: Duration,
}

impl Default for PushConfig {
    fn default() -> Self {
        Self {
            reconnect_min: Duration::from_secs(1),
            reconnect_max: Duration::from_secs(60),
            max_stream_age: Duration::from_secs(600),
        }
    }
}

/// Wakes a poller when the server announces new mail.
#[derive(Debug, Clone)]
pub struct PushWatcher {
    notify: Arc<Notify>,
}

impl PushWatcher {
    /// Resolves when mail may have arrived (or the stream just (re)connected). Remembers
    /// one pending wake-up, so an event during a poll is not lost.
    pub async fn changed(&self) {
        self.notify.notified().await;
    }

    /// Start watching in the background until `shutdown` flips to `true`.
    #[must_use]
    pub fn start(
        settings: Arc<JmapSettings>,
        config: PushConfig,
        shutdown: watch::Receiver<bool>,
    ) -> Self {
        let notify = Arc::new(Notify::new());
        tokio::spawn(run(settings, config, Arc::clone(&notify), shutdown));
        Self { notify }
    }
}

async fn run(
    settings: Arc<JmapSettings>,
    config: PushConfig,
    notify: Arc<Notify>,
    mut shutdown: watch::Receiver<bool>,
) {
    let mut backoff = config.reconnect_min;
    loop {
        let outcome = tokio::select! {
            () = async { let _ = shutdown.wait_for(|stop| *stop).await; } => return,
            result = stream_once(&settings, &config, &notify) => result,
        };
        match outcome {
            Ok(()) => backoff = config.reconnect_min,
            Err(e) => {
                tracing::warn!(error = %e, retry_in = ?backoff, "mail push stream lost");
                backoff = (backoff * 2).min(config.reconnect_max);
            }
        }
        tokio::select! {
            () = async { let _ = shutdown.wait_for(|stop| *stop).await; } => return,
            () = tokio::time::sleep(backoff) => {}
        }
    }
}

/// One connection. `Ok` means it ended routinely (closed, or rotated at its timeout).
async fn stream_once(
    settings: &JmapSettings,
    config: &PushConfig,
    notify: &Notify,
) -> Result<(), JmapError> {
    let client = connect(settings, settings.timeout).await?;
    let mut stream = client
        .event_source(Some([DataType::Email]), false, Some(30), None)
        .await
        .map_err(JmapError::from)?;
    // Anything announced while we were disconnected is covered by a poll now.
    notify.notify_one();
    let expiry = tokio::time::Instant::now() + config.max_stream_age;
    loop {
        // Running into the maximum age is how a healthy stream is meant to end.
        let Ok(item) = tokio::time::timeout_at(expiry, stream.next()).await else {
            return Ok(());
        };
        match item {
            Some(Ok(PushNotification::StateChange(changes)))
                if changes.has_type(DataType::Email) =>
            {
                notify.notify_one();
            }
            Some(Ok(_)) => {}
            Some(Err(e)) => return Err(e.into()),
            None => return Ok(()),
        }
    }
}
