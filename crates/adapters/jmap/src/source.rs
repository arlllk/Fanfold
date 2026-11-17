//! [`MailSource`] over JMAP.
//!
//! * **First run** (no saved cursor): read the current state, then the newest messages
//!   from the last `backfill` window, so mail that arrived while the service was not yet
//!   running is not lost.
//! * **Afterwards**: `Email/changes` since the saved state: cheap, exact, and resumable
//!   across restarts. A state the server no longer recognises falls back to a backfill,
//!   which is safe because handling a message twice is idempotent.
//!
//! Nothing here modifies the mailbox: no flags are set and nothing is moved.

use std::{sync::Arc, time::Duration};

use async_trait::async_trait;
use fanfold_app::{
    error::{MailError, RepoError},
    ports::{Checkpoint, Clock, CursorStore, MailBatch, MailSource},
};
use jmap_client::{
    client::Client,
    core::query::Filter,
    email::{Email, Property, query::Comparator, query::Filter as EmailFilter},
    mailbox::{Role, query::Filter as MailboxFilter},
};
use tokio::sync::Mutex;

use crate::{JmapError, JmapSettings, connect, convert::to_message};

/// The cursor-store key holding the last committed `Email` state.
pub const CURSOR_KEY: &str = "jmap.email_state";

/// Largest body part requested. AO3 notifications are about 8-25 kB.
const MAX_BODY_BYTES: usize = 256 * 1024;

pub struct JmapMailSource {
    client: Arc<Client>,
    cursors: Arc<dyn CursorStore>,
    clock: Arc<dyn Clock>,
    backfill: Duration,
    batch_size: u32,
    inbox: Mutex<Option<String>>,
}

impl std::fmt::Debug for JmapMailSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("JmapMailSource").finish_non_exhaustive()
    }
}

fn backend(e: &JmapError) -> MailError {
    MailError::Backend(e.to_string())
}

fn cursor_error(e: &RepoError) -> MailError {
    MailError::Backend(format!("mail cursor: {e}"))
}

impl JmapMailSource {
    /// `batch_size` is capped at the server's `maxObjectsInGet` (500 for Stalwart).
    #[must_use]
    pub fn new(
        client: Arc<Client>,
        cursors: Arc<dyn CursorStore>,
        clock: Arc<dyn Clock>,
        backfill: Duration,
        batch_size: u32,
    ) -> Self {
        Self {
            client,
            cursors,
            clock,
            backfill,
            batch_size: batch_size.clamp(1, 500),
            inbox: Mutex::new(None),
        }
    }

    async fn inbox_id(&self) -> Result<String, JmapError> {
        let mut cached = self.inbox.lock().await;
        if let Some(id) = cached.as_ref() {
            return Ok(id.clone());
        }
        let mut found = self
            .client
            .mailbox_query(Some(MailboxFilter::role(Role::Inbox)), None::<Vec<_>>)
            .await?;
        let id = found
            .take_ids()
            .pop()
            .ok_or_else(|| JmapError::Malformed("the account has no inbox".into()))?;
        *cached = Some(id.clone());
        Ok(id)
    }

    /// The current `Email` state, without fetching any email.
    async fn current_state(&self) -> Result<String, JmapError> {
        let mut request = self.client.build();
        request
            .get_email()
            .ids(Vec::<String>::new())
            .properties([Property::Id]);
        Ok(request
            .send_single::<jmap_client::core::response::EmailGetResponse>()
            .await?
            .take_state())
    }

    /// Fetch these emails with their bodies, oldest first.
    ///
    /// `bodyValues` must be requested explicitly in `properties`: asking for
    /// `fetchTextBodyValues` alone returns no bodies (found against a real server).
    async fn load(
        &self,
        ids: Vec<String>,
        inbox: &str,
    ) -> Result<Vec<fanfold_app::ports::MailMessage>, JmapError> {
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        let mut request = self.client.build();
        let get = request.get_email();
        get.ids(ids).properties([
            Property::Id,
            Property::From,
            Property::Subject,
            Property::MailboxIds,
            Property::ReceivedAt,
            Property::TextBody,
            Property::HtmlBody,
            Property::BodyValues,
        ]);
        get.arguments()
            .fetch_text_body_values(true)
            .fetch_html_body_values(true)
            .max_body_value_bytes(MAX_BODY_BYTES);
        let mut emails: Vec<Email> = request
            .send_single::<jmap_client::core::response::EmailGetResponse>()
            .await?
            .take_list();
        emails.sort_by_key(|e| (e.received_at(), e.id().map(str::to_owned)));
        Ok(emails.iter().filter_map(|e| to_message(e, inbox)).collect())
    }

    async fn backfilled(&self, inbox: &str) -> Result<MailBatch, JmapError> {
        // The state *before* the query: anything delivered since is reported again by the
        // next `Email/changes`. Overlap is harmless; a gap would lose mail.
        let state = self.current_state().await?;
        let after = self
            .clock
            .now()
            .checked_sub(
                jiff::SignedDuration::try_from(self.backfill).unwrap_or(jiff::SignedDuration::MAX),
            )
            .unwrap_or(jiff::Timestamp::MIN)
            .as_second();
        let mut request = self.client.build();
        request
            .query_email()
            .filter(Filter::and([
                EmailFilter::in_mailbox(inbox),
                EmailFilter::after(after),
            ]))
            // Newest first, so a huge backlog keeps the recent mail.
            .sort([Comparator::received_at().descending()])
            .limit(self.batch_size as usize);
        let ids = request
            .send_single::<jmap_client::core::query::QueryResponse>()
            .await?
            .take_ids();
        let messages = self.load(ids, inbox).await?;
        Ok(MailBatch {
            messages,
            checkpoint: Checkpoint(state),
        })
    }

    async fn changed(&self, inbox: &str, since: &str) -> Result<MailBatch, JmapError> {
        let changes = self
            .client
            .email_changes(since, Some(self.batch_size as usize))
            .await?;
        if changes.has_more_changes() {
            tracing::debug!("more mail changes are waiting; the next poll continues");
        }
        let messages = self.load(changes.created().to_vec(), inbox).await?;
        Ok(MailBatch {
            messages,
            checkpoint: Checkpoint(changes.new_state().to_owned()),
        })
    }

    async fn fetch(&self) -> Result<MailBatch, MailError> {
        let inbox = self.inbox_id().await.map_err(|e| backend(&e))?;
        let saved = self
            .cursors
            .get(CURSOR_KEY)
            .await
            .map_err(|e| cursor_error(&e))?;
        let Some(state) = saved else {
            tracing::info!(window = ?self.backfill, "no saved mail cursor: reading recent mail");
            return self.backfilled(&inbox).await.map_err(|e| backend(&e));
        };
        match self.changed(&inbox, &state).await {
            Ok(batch) => Ok(batch),
            Err(JmapError::StateUnusable) => {
                tracing::warn!(
                    "the saved mail cursor is no longer valid: reading recent mail again"
                );
                self.backfilled(&inbox).await.map_err(|e| backend(&e))
            }
            Err(e) => Err(backend(&e)),
        }
    }
}

#[async_trait]
impl MailSource for JmapMailSource {
    async fn fetch_new(&self) -> Result<MailBatch, MailError> {
        self.fetch().await
    }

    async fn commit(&self, checkpoint: &Checkpoint) -> Result<(), MailError> {
        self.cursors
            .set(CURSOR_KEY, &checkpoint.0)
            .await
            .map_err(|e| cursor_error(&e))
    }
}

/// A [`MailSource`] that connects on demand and reconnects after any failure.
///
/// `jmap-client` cannot build a client until the server answers, so without this a
/// mail-server outage at start-up would stop the service, and a restarted server would
/// leave a stale session behind. Here the first poll that can connect simply starts
/// working, and after an error the next poll opens a fresh session.
pub struct ResilientMailSource {
    settings: Arc<JmapSettings>,
    cursors: Arc<dyn CursorStore>,
    clock: Arc<dyn Clock>,
    backfill: Duration,
    batch_size: u32,
    inner: Mutex<Option<Arc<JmapMailSource>>>,
}

impl std::fmt::Debug for ResilientMailSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ResilientMailSource")
            .field("settings", &self.settings)
            .finish_non_exhaustive()
    }
}

impl ResilientMailSource {
    #[must_use]
    pub fn new(
        settings: Arc<JmapSettings>,
        cursors: Arc<dyn CursorStore>,
        clock: Arc<dyn Clock>,
        backfill: Duration,
        batch_size: u32,
    ) -> Self {
        Self {
            settings,
            cursors,
            clock,
            backfill,
            batch_size,
            inner: Mutex::new(None),
        }
    }

    async fn connected(&self) -> Result<Arc<JmapMailSource>, JmapError> {
        let mut slot = self.inner.lock().await;
        if let Some(source) = slot.as_ref() {
            return Ok(Arc::clone(source));
        }
        let client = Arc::new(connect(&self.settings, self.settings.timeout).await?);
        let source = Arc::new(JmapMailSource::new(
            client,
            Arc::clone(&self.cursors),
            Arc::clone(&self.clock),
            self.backfill,
            self.batch_size,
        ));
        *slot = Some(Arc::clone(&source));
        Ok(source)
    }
}

#[async_trait]
impl MailSource for ResilientMailSource {
    async fn fetch_new(&self) -> Result<MailBatch, MailError> {
        let source = self.connected().await.map_err(|e| backend(&e))?;
        let result = source.fetch_new().await;
        if result.is_err() {
            // Whatever went wrong, start the next attempt from a fresh session.
            *self.inner.lock().await = None;
        }
        result
    }

    /// Needs no connection: the cursor lives in our own store.
    async fn commit(&self, checkpoint: &Checkpoint) -> Result<(), MailError> {
        self.cursors
            .set(CURSOR_KEY, &checkpoint.0)
            .await
            .map_err(|e| cursor_error(&e))
    }
}
