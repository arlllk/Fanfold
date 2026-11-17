//! Live change notifications over PostgreSQL `LISTEN/NOTIFY`.
//!
//! Triggers (migration 0002) announce job and log changes. One listener connection
//! fans them out to any number of in-process subscribers. Notifications are not
//! durable, so after any reconnect subscribers receive [`Event::Resync`] and refetch.
//! Needs a session-level connection: it works through a connection pooler only when the
//! pooler runs in session mode (a transaction-mode pooler drops `LISTEN`).

use std::time::Duration;

use fanfold_app::{
    error::RepoError,
    ports::{Event, EventSource},
};
use fanfold_domain::job::JobId;
use sqlx::{PgPool, postgres::PgListener};
use tokio::sync::{broadcast, watch};

pub const CHANNEL: &str = "fanfold_events";
const MAX_BACKOFF: Duration = Duration::from_secs(30);

/// Decode a trigger payload. Unknown shapes yield `None` and are ignored.
#[must_use]
pub fn parse_payload(payload: &str) -> Option<Event> {
    let (kind, id) = payload.split_once(':')?;
    let id = JobId::new(id.parse().ok()?);
    match kind {
        "j" => Some(Event::JobChanged(id)),
        "l" => Some(Event::LogAppended(id)),
        _ => None,
    }
}

#[derive(Debug, Clone)]
pub struct PgEvents {
    tx: broadcast::Sender<Event>,
}

impl PgEvents {
    /// Connect, start listening, and spawn the fan-out task. Returns only once the
    /// subscription is active, so no change made afterwards is missed.
    pub async fn start(pool: PgPool, shutdown: watch::Receiver<bool>) -> Result<Self, RepoError> {
        let mut listener = connect(&pool)
            .await
            .map_err(|e| RepoError::Backend(e.to_string()))?;
        let (tx, _) = broadcast::channel(256);
        let events = Self { tx: tx.clone() };
        tokio::spawn(async move {
            run(&pool, &mut listener, &tx, shutdown).await;
        });
        Ok(events)
    }
}

impl EventSource for PgEvents {
    fn subscribe(&self) -> broadcast::Receiver<Event> {
        self.tx.subscribe()
    }
}

async fn connect(pool: &PgPool) -> Result<PgListener, sqlx::Error> {
    let mut listener = PgListener::connect_with(pool).await?;
    listener.listen(CHANNEL).await?;
    Ok(listener)
}

async fn run(
    pool: &PgPool,
    listener: &mut PgListener,
    tx: &broadcast::Sender<Event>,
    mut shutdown: watch::Receiver<bool>,
) {
    let mut backoff = Duration::from_secs(1);
    loop {
        // Forward until the connection fails.
        loop {
            tokio::select! {
                () = async { let _ = shutdown.wait_for(|stop| *stop).await; } => return,
                received = listener.try_recv() => match received {
                    Ok(Some(note)) => {
                        if let Some(event) = parse_payload(note.payload()) {
                            let _ = tx.send(event);
                        }
                    }
                    // sqlx reconnected on its own; anything sent meanwhile is lost.
                    Ok(None) => {
                        let _ = tx.send(Event::Resync);
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "event listener lost its connection");
                        break;
                    }
                },
            }
        }
        // Reconnect with back-off.
        loop {
            tokio::select! {
                () = async { let _ = shutdown.wait_for(|stop| *stop).await; } => return,
                () = tokio::time::sleep(backoff) => {}
            }
            match connect(pool).await {
                Ok(fresh) => {
                    *listener = fresh;
                    backoff = Duration::from_secs(1);
                    let _ = tx.send(Event::Resync);
                    break;
                }
                Err(e) => {
                    tracing::warn!(error = %e, retry_in = ?backoff, "event listener reconnect failed");
                    backoff = (backoff * 2).min(MAX_BACKOFF);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_trigger_payloads() {
        assert_eq!(
            parse_payload("j:42"),
            Some(Event::JobChanged(JobId::new(42)))
        );
        assert_eq!(
            parse_payload("l:7"),
            Some(Event::LogAppended(JobId::new(7)))
        );
    }

    #[test]
    fn ignores_anything_else() {
        for bad in ["", "j", "j:", "j:abc", "x:1", ":1", "j:1:2", "J:1", " j:1"] {
            assert_eq!(parse_payload(bad), None, "{bad:?}");
        }
    }
}
