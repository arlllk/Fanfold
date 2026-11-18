//! Behaviour of the mail trigger against a REAL mailbox, through `jmap-client`. Read-only.
//!
//! The real [`JmapMailSource`] and the real [`Ingestor`] run against a live JMAP server,
//! with in-memory jobs and cursor, so nothing reaches AO3 and no message is changed.
//!
//! From a laptop the server's advertised address is unreachable, so the test goes through
//! a local proxy that rewrites it (see `common/proxy.rs`); in the cluster DNS does that.
//!
//! ```sh
//! JMAP_UPSTREAM=http://127.0.0.1:18081 JMAP_PUBLIC_ORIGIN=https://mx.example.org \
//! JMAP_USER=ficbot@example.org JMAP_PASSWORD=... \
//!   cargo test -p fanfold-adapter-jmap --test live -- --ignored --nocapture
//! ```

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::print_stderr,
    clippy::clone_on_ref_ptr,
    clippy::indexing_slicing
)]

#[allow(dead_code)]
mod common;

use std::{collections::BTreeSet, sync::Arc, time::Duration};

use common::proxy::SessionProxy;
use fanfold_adapter_jmap::{
    CURSOR_KEY, JmapMailSource, JmapSettings, PushConfig, PushWatcher, connect,
};
use fanfold_app::{
    ingest::{IngestConfig, Ingestor},
    ports::{Clock, CursorStore, MailSource},
};
use fanfold_domain::{
    Timestamp,
    job::{JobSource, JobState},
    mail::extract_work_ids,
    work::WorkId,
};
use fanfold_testkit::memory::{InMemoryCursors, InMemoryJobs};
use secrecy::SecretString;
use tokio::sync::watch;

struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> Timestamp {
        Timestamp::now()
    }
}

fn env(name: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| panic!("set {name}"))
}

async fn settings() -> (SessionProxy, Arc<JmapSettings>) {
    let proxy = SessionProxy::start(&env("JMAP_UPSTREAM"), &env("JMAP_PUBLIC_ORIGIN")).await;
    let settings = Arc::new(JmapSettings {
        base_url: proxy.base.clone(),
        username: env("JMAP_USER"),
        password: SecretString::from(env("JMAP_PASSWORD")),
        timeout: Duration::from_secs(30),
    });
    (proxy, settings)
}

#[tokio::test]
// One long scenario on purpose: each step builds on the previous one.
#[allow(clippy::too_many_lines)]
#[ignore = "needs a live JMAP server: JMAP_UPSTREAM, JMAP_PUBLIC_ORIGIN, JMAP_USER, JMAP_PASSWORD"]
async fn the_real_mailbox_produces_the_right_downloads() {
    let (_proxy, settings) = settings().await;
    let client = Arc::new(
        connect(&settings, Duration::from_secs(30))
            .await
            .expect("jmap-client connects"),
    );
    let cursors = Arc::new(InMemoryCursors::new());
    let source = Arc::new(JmapMailSource::new(
        client,
        cursors.clone(),
        Arc::new(SystemClock),
        Duration::from_hours(365 * 24),
        200,
    ));
    let allowed = vec!["archiveofourown.org".to_owned()];

    // 1. What does the mailbox hold? (Fetching has no side effects until commit.)
    let batch = source.fetch_new().await.expect("the mailbox is readable");
    assert!(
        !batch.messages.is_empty(),
        "the test mailbox should not be empty"
    );
    let ao3: Vec<_> = batch
        .messages
        .iter()
        .filter(|m| fanfold_app::ingest::sender_allowed(&m.from, &allowed))
        .collect();
    let other = batch.messages.len() - ao3.len();
    // The independent oracle: every work id linked from an AO3 message, and how many links.
    let per_message: Vec<_> = ao3
        .iter()
        .map(|m| extract_work_ids(&format!("{}\n{}", m.text, m.html)))
        .collect();
    let linked: usize = per_message.iter().map(Vec::len).sum();
    let expected: BTreeSet<u64> = per_message.iter().flatten().map(|w| w.get()).collect();
    eprintln!(
        "mailbox: {} messages = {} from AO3 + {} others; {} work links across AO3 mail -> {} distinct works {:?}",
        batch.messages.len(),
        ao3.len(),
        other,
        linked,
        expected.len(),
        expected
    );

    for m in &batch.messages {
        assert!(
            !m.id.is_empty() && !m.from.is_empty() && !m.subject.is_empty(),
            "complete headers: {m:?}"
        );
        assert!(
            !m.text.is_empty() || !m.html.is_empty(),
            "a body for {}",
            m.id
        );
    }
    assert!(!ao3.is_empty(), "the mailbox should hold AO3 notifications");
    assert!(
        per_message.iter().all(|ids| ids.len() == 1),
        "each AO3 chapter notification links exactly one work: {:?}",
        per_message.iter().map(Vec::len).collect::<Vec<_>>()
    );
    assert!(
        ao3.iter().all(|m| m.subject.starts_with("[AO3]")),
        "AO3 mail is recognisable by subject too"
    );

    // 2. Run the real trigger.
    let jobs = Arc::new(InMemoryJobs::new());
    let ingestor = Ingestor::new(
        source.clone(),
        jobs.clone(),
        jobs.clone(),
        Arc::new(SystemClock),
        IngestConfig {
            allowed_senders: allowed,
            max_links_per_message: 20,
        },
    );
    let report = ingestor.run_once().await.expect("ingest succeeds");
    eprintln!("first run:  {report:?}");

    assert_eq!(report.messages, batch.messages.len());
    assert_eq!(
        report.from_unlisted_sender, other,
        "non-AO3 mail is ignored"
    );
    assert_eq!(report.without_work_links, 0);
    assert_eq!(report.truncated_links, 0);
    assert_eq!(report.enqueued, expected.len(), "one job per distinct work");
    assert_eq!(
        report.already_active,
        linked - expected.len(),
        "repeat chapters of a work collapse into one job"
    );

    let queued = jobs.all_jobs();
    assert_eq!(
        queued
            .iter()
            .filter_map(|j| j.target.work())
            .map(WorkId::get)
            .collect::<BTreeSet<_>>(),
        expected
    );
    assert!(
        queued
            .iter()
            .all(|j| j.state == JobState::Queued && j.source == JobSource::Mail)
    );

    // 3. The cursor was committed, and a second poll finds nothing new.
    let saved = cursors.get(CURSOR_KEY).await.unwrap();
    assert_eq!(
        saved.as_deref(),
        Some(batch.checkpoint.0.as_str()),
        "the cursor is the state the batch was read at"
    );
    let again = ingestor.run_once().await.expect("second poll succeeds");
    eprintln!("second run: {again:?}");
    assert_eq!(
        again.messages, 0,
        "nothing new since the cursor (unless mail arrived in the last second)"
    );
    assert_eq!(jobs.all_jobs().len(), expected.len(), "no duplicate jobs");

    // 4. A fresh process with the saved cursor does not replay the mailbox either.
    let client = Arc::new(connect(&settings, Duration::from_secs(30)).await.unwrap());
    let restarted = JmapMailSource::new(
        client,
        cursors.clone(),
        Arc::new(SystemClock),
        Duration::from_secs(3600),
        200,
    );
    let after_restart = restarted.fetch_new().await.expect("poll after restart");
    assert!(
        after_restart.messages.is_empty(),
        "a restart resumes from the cursor instead of replaying"
    );
    eprintln!(
        "after restart: {} messages (resumed from the saved cursor)",
        after_restart.messages.len()
    );
}

#[tokio::test]
#[ignore = "needs a live JMAP server: JMAP_UPSTREAM, JMAP_PUBLIC_ORIGIN, JMAP_USER, JMAP_PASSWORD"]
async fn the_real_servers_event_stream_connects_and_wakes_the_poller() {
    let (_proxy, settings) = settings().await;
    let (_stop, stopped) = watch::channel(false);
    let watcher = PushWatcher::start(settings, PushConfig::default(), stopped);
    tokio::time::timeout(Duration::from_secs(15), watcher.changed())
        .await
        .expect("connecting to the real event stream wakes the poller once");
    // No new mail is being delivered, so it should now be quiet.
    assert!(
        tokio::time::timeout(Duration::from_secs(2), watcher.changed())
            .await
            .is_err()
    );
    eprintln!("real event stream: connected, quiet (pings only)");
}
