//! The JMAP mail source and the ingest trigger, against a fake server that enforces the
//! rules the real one does. (The same trigger is also run against a real mailbox in
//! `live.rs`.)

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::clone_on_ref_ptr,
    clippy::indexing_slicing
)]

#[allow(dead_code)]
mod common;

use std::{collections::BTreeSet, sync::Arc, time::Duration};

use common::{FakeJmap, INBOX, MailSpec, SENT};
use fanfold_adapter_jmap::{
    CURSOR_KEY, JmapError, JmapMailSource, JmapSettings, ResilientMailSource, connect,
};
use fanfold_app::{
    error::RepoError,
    ingest::{IngestConfig, Ingestor},
    ports::{Clock, CursorStore, MailSource},
};
use fanfold_domain::{Timestamp, job::JobState};
use fanfold_testkit::memory::{FakeClock, InMemoryCursors, InMemoryJobs};
use jmap_client::client::Client;
use secrecy::SecretString;

const USER: &str = "ficbot@example.org";
const PASSWORD: &str = "s3cret-pw-do-not-leak";
const NOW: &str = "2026-11-16T12:00:00Z";

struct Rig {
    fake: FakeJmap,
    cursors: Arc<InMemoryCursors>,
    clock: Arc<FakeClock>,
    source: Arc<JmapMailSource>,
}

fn settings(fake: &FakeJmap, password: &str) -> JmapSettings {
    JmapSettings {
        base_url: fake.base.clone(),
        username: USER.to_owned(),
        password: SecretString::from(password.to_owned()),
        timeout: Duration::from_secs(5),
    }
}

async fn client(fake: &FakeJmap, password: &str) -> Arc<Client> {
    Arc::new(
        connect(&settings(fake, password), Duration::from_secs(5))
            .await
            .unwrap(),
    )
}

async fn rig_with(batch: u32, password: &str) -> Rig {
    let fake = FakeJmap::start(USER, PASSWORD).await;
    let cursors = Arc::new(InMemoryCursors::new());
    let clock = Arc::new(FakeClock::at_secs(
        NOW.parse::<Timestamp>().unwrap().as_second(),
    ));
    let source = Arc::new(JmapMailSource::new(
        client(&fake, password).await,
        cursors.clone(),
        clock.clone(),
        Duration::from_hours(7 * 24),
        batch,
    ));
    Rig {
        fake,
        cursors,
        clock,
        source,
    }
}

async fn rig() -> Rig {
    rig_with(200, PASSWORD).await
}

fn ids(batch: &fanfold_app::ports::MailBatch) -> Vec<&str> {
    batch.messages.iter().map(|m| m.id.as_str()).collect()
}

#[tokio::test]
async fn it_connects_through_discovery_and_sends_credentials_only_as_basic_auth() {
    let rig = rig().await;
    rig.source.fetch_new().await.unwrap();

    let requests = rig.fake.requests();
    let paths: Vec<(&str, &str)> = requests
        .iter()
        .map(|r| (r.method.as_str(), r.path.as_str()))
        .collect();
    assert_eq!(
        paths[..3],
        [
            ("GET", "/.well-known/jmap"),
            ("GET", "/jmap/session"),
            ("POST", "/jmap/")
        ],
        "discovery follows the redirect to the session document, then uses the advertised API address"
    );
    for r in &requests {
        assert!(
            r.headers["authorization"].starts_with("Basic "),
            "{} {}",
            r.method,
            r.path
        );
        assert!(
            !r.body.contains(PASSWORD) && !r.path.contains(PASSWORD),
            "the password only travels in the header"
        );
    }
}

#[tokio::test]
async fn the_first_poll_reads_recent_inbox_mail_oldest_first_and_its_bodies() {
    let rig = rig().await;
    rig.fake
        .deliver(MailSpec::ao3_chapter(1, 1, "2026-11-01T00:00:00Z")); // older than the window
    let a1 = rig
        .fake
        .deliver(MailSpec::ao3_chapter(100, 1, "2026-11-10T01:00:00Z"));
    let other = rig.fake.deliver(MailSpec::other(
        "Confirm forwarding request",
        "2026-11-13T09:00:00Z",
    ));
    let a2 = rig
        .fake
        .deliver(MailSpec::ao3_chapter(100, 2, "2026-11-12T01:00:00Z"));
    rig.fake
        .deliver(MailSpec::ao3_chapter(300, 1, "2026-11-15T00:00:00Z").in_mailbox(SENT)); // not the inbox
    let state_before = rig.fake.state();

    let batch = rig.source.fetch_new().await.unwrap();

    assert_eq!(
        ids(&batch),
        [a1.as_str(), a2.as_str(), other.as_str()],
        "inbox only, inside the window, oldest first"
    );
    assert_eq!(
        batch.checkpoint.0, state_before,
        "the checkpoint is the state the batch was read at"
    );

    // Regression: bodies must actually come back (the server only sends bodyValues when asked).
    let first = &batch.messages[0];
    assert!(
        first
            .text
            .contains("https://archiveofourown.org/works/100/chapters/10"),
        "{}",
        first.text
    );
    assert!(first.html.contains("works/100"), "{}", first.html);
    assert_eq!(
        first.from,
        "Archive of Our Own - do-not-reply at archiveofourown.org <do-not-reply_at_archiveofourown_org_cqcc@relay.example>"
    );
    assert_eq!(first.subject, "[AO3] Someone posted Chapter 1 of A Story");
    assert_eq!(
        batch.messages[2].from, "no-reply@mail.proton.me",
        "no display name: just the address"
    );

    // The state is read BEFORE the query, so mail delivered in between is not lost.
    assert_eq!(
        rig.fake.api_methods(),
        ["Mailbox/query", "Email/get", "Email/query", "Email/get"]
    );
    assert_eq!(
        rig.cursors.get(CURSOR_KEY).await.unwrap(),
        None,
        "fetching alone changes nothing"
    );
}

#[tokio::test]
async fn committing_saves_the_cursor_and_later_polls_only_ask_for_changes() {
    let rig = rig().await;
    rig.fake
        .deliver(MailSpec::ao3_chapter(100, 1, "2026-11-10T01:00:00Z"));
    let first = rig.source.fetch_new().await.unwrap();
    rig.source.commit(&first.checkpoint).await.unwrap();
    assert_eq!(
        rig.cursors.get(CURSOR_KEY).await.unwrap().as_deref(),
        Some(first.checkpoint.0.as_str())
    );

    let n1 = rig
        .fake
        .deliver(MailSpec::ao3_chapter(100, 2, "2026-11-16T11:00:00Z"));
    let n2 = rig
        .fake
        .deliver(MailSpec::ao3_chapter(200, 1, "2026-11-16T11:30:00Z"));
    let calls_before = rig.fake.api_methods().len();
    let second = rig.source.fetch_new().await.unwrap();
    assert_eq!(
        ids(&second),
        [n1.as_str(), n2.as_str()],
        "only what arrived since"
    );
    assert_eq!(second.checkpoint.0, rig.fake.state());
    assert_eq!(
        rig.fake.api_methods()[calls_before..],
        ["Email/changes", "Email/get"],
        "no re-query of the mailbox"
    );

    rig.source.commit(&second.checkpoint).await.unwrap();
    let calls_before = rig.fake.api_methods().len();
    let third = rig.source.fetch_new().await.unwrap();
    assert_eq!(third.messages, Vec::new());
    assert_eq!(third.checkpoint, second.checkpoint);
    assert_eq!(
        rig.fake.api_methods()[calls_before..],
        ["Email/changes"],
        "nothing new: one cheap call and no fetch"
    );
}

#[tokio::test]
async fn mail_filed_elsewhere_is_ignored_but_the_cursor_still_advances() {
    let rig = rig().await;
    let first = rig.source.fetch_new().await.unwrap();
    rig.source.commit(&first.checkpoint).await.unwrap();
    rig.fake
        .deliver(MailSpec::ao3_chapter(5, 1, "2026-11-16T11:00:00Z").in_mailbox(SENT));
    let batch = rig.source.fetch_new().await.unwrap();
    assert_eq!(batch.messages, Vec::new());
    assert_eq!(batch.checkpoint.0, rig.fake.state());
}

#[tokio::test]
async fn an_uncommitted_batch_is_redelivered() {
    let rig = rig().await;
    let id = rig
        .fake
        .deliver(MailSpec::ao3_chapter(100, 1, "2026-11-10T01:00:00Z"));
    let a = rig.source.fetch_new().await.unwrap();
    let b = rig.source.fetch_new().await.unwrap(); // the process "crashed" before committing
    assert_eq!((ids(&a), ids(&b)), (vec![id.as_str()], vec![id.as_str()]));
    assert_eq!(a.checkpoint, b.checkpoint);
}

#[tokio::test]
async fn an_expired_or_garbled_cursor_falls_back_to_reading_recent_mail() {
    for saved in [None, Some("garbage"), Some("")] {
        let rig = rig().await;
        let seen = rig
            .fake
            .deliver(MailSpec::ao3_chapter(100, 1, "2026-11-10T01:00:00Z"));
        let warmup = rig.source.fetch_new().await.unwrap();
        rig.source.commit(&warmup.checkpoint).await.unwrap();
        let fresh = rig
            .fake
            .deliver(MailSpec::ao3_chapter(200, 1, "2026-11-16T10:00:00Z"));
        match saved {
            None => rig.fake.expire_states(), // too old to calculate changes from
            Some(bad) => rig.cursors.set(CURSOR_KEY, bad).await.unwrap(), // not a state this server issued
        }
        let calls_before = rig.fake.api_methods().len();

        let batch = rig.source.fetch_new().await.unwrap();
        assert_eq!(
            ids(&batch),
            [seen.as_str(), fresh.as_str()],
            "{saved:?}: recent mail is read again (handling it twice is idempotent)"
        );
        assert_eq!(batch.checkpoint.0, rig.fake.state());
        assert_eq!(
            rig.fake.api_methods()[calls_before..],
            ["Email/changes", "Email/get", "Email/query", "Email/get"],
            "{saved:?}: tried the cursor first, then recovered"
        );
    }
}

#[tokio::test]
async fn big_bursts_are_read_in_bounded_batches_without_loss_or_repeats() {
    let rig = rig_with(2, PASSWORD).await;
    let warmup = rig.source.fetch_new().await.unwrap();
    rig.source.commit(&warmup.checkpoint).await.unwrap();
    let delivered: Vec<String> = (1..=5)
        .map(|n| {
            rig.fake.deliver(MailSpec::ao3_chapter(
                n,
                1,
                &format!("2026-11-16T0{n}:00:00Z"),
            ))
        })
        .collect();

    let mut seen = Vec::new();
    let mut sizes = Vec::new();
    for _ in 0..6 {
        let batch = rig.source.fetch_new().await.unwrap();
        if batch.messages.is_empty() {
            break;
        }
        sizes.push(batch.messages.len());
        seen.extend(batch.messages.iter().map(|m| m.id.clone()));
        rig.source.commit(&batch.checkpoint).await.unwrap();
    }
    assert_eq!(sizes, [2, 2, 1], "never more than the batch size");
    assert_eq!(seen, delivered, "every message exactly once, in order");
}

#[tokio::test]
async fn bad_credentials_are_reported_and_the_password_never_leaks() {
    let fake = FakeJmap::start(USER, PASSWORD).await;
    let wrong = settings(&fake, "the-wrong-password");
    let Err(err) = connect(&wrong, Duration::from_secs(5)).await else {
        panic!("bad credentials must be rejected")
    };
    assert_eq!(err, JmapError::Auth);
    for text in [err.to_string(), format!("{err:?}"), format!("{wrong:?}")] {
        assert!(
            !text.contains("the-wrong-password") && !text.contains(PASSWORD),
            "{text}"
        );
    }
    // The same goes for a source built on good credentials.
    let rig = rig().await;
    assert!(!format!("{:?}", rig.source).contains(PASSWORD));
}

#[tokio::test]
async fn a_server_error_is_retryable_and_changes_nothing() {
    let rig = rig().await;
    let batch = rig.source.fetch_new().await.unwrap();
    rig.source.commit(&batch.checkpoint).await.unwrap();
    let saved = rig.cursors.get(CURSOR_KEY).await.unwrap();

    rig.fake.fail_api(Some(500));
    let err = rig.source.fetch_new().await.unwrap_err();
    assert!(err.to_string().contains("500"), "{err}");
    assert_eq!(
        rig.cursors.get(CURSOR_KEY).await.unwrap(),
        saved,
        "the cursor is untouched"
    );

    rig.fake.fail_api(None);
    rig.fake
        .deliver(MailSpec::ao3_chapter(7, 1, "2026-11-16T11:00:00Z"));
    assert_eq!(
        rig.source.fetch_new().await.unwrap().messages.len(),
        1,
        "and it recovers"
    );
}

#[tokio::test]
async fn cursor_storage_failures_surface_instead_of_being_swallowed() {
    let rig = rig().await;
    rig.cursors
        .break_with(Some(RepoError::Backend("db down".into())));
    let err = rig.source.fetch_new().await.unwrap_err();
    assert!(err.to_string().contains("mail cursor"), "{err}");
    let batch = {
        rig.cursors.break_with(None);
        rig.source.fetch_new().await.unwrap()
    };
    rig.cursors
        .break_with(Some(RepoError::Backend("db down".into())));
    assert!(rig.source.commit(&batch.checkpoint).await.is_err());
}

// ---------------------------------------------------------------- the whole trigger

fn ingestor(rig: &Rig, jobs: &Arc<InMemoryJobs>) -> Ingestor {
    Ingestor::new(
        rig.source.clone(),
        jobs.clone(),
        rig.clock.clone() as Arc<dyn Clock>,
        IngestConfig {
            allowed_senders: vec!["archiveofourown.org".to_owned()],
            max_links_per_message: 20,
        },
    )
}

#[tokio::test]
async fn the_trigger_turns_a_backlog_into_one_job_per_work() {
    let rig = rig().await;
    let jobs = Arc::new(InMemoryJobs::new());
    // Four weeks of queued notifications arrive at once: three chapters of one work, two of
    // another, and a forwarding confirmation that must be ignored.
    for ch in 1..=3 {
        rig.fake.deliver(MailSpec::ao3_chapter(
            100,
            ch,
            &format!("2026-11-1{ch}T01:00:00Z"),
        ));
    }
    for ch in 1..=2 {
        rig.fake.deliver(MailSpec::ao3_chapter(
            200,
            ch,
            &format!("2026-11-1{}T05:00:00Z", ch + 3),
        ));
    }
    rig.fake.deliver(MailSpec::other(
        "Confirm forwarding request",
        "2026-11-15T09:00:00Z",
    ));

    let report = ingestor(&rig, &jobs).run_once().await.unwrap();
    assert_eq!(
        (
            report.messages,
            report.enqueued,
            report.already_active,
            report.from_unlisted_sender,
            report.without_work_links
        ),
        (6, 2, 3, 1, 0),
        "{report:?}"
    );
    let works: BTreeSet<u64> = jobs.all_jobs().iter().map(|j| j.work.get()).collect();
    assert_eq!(works, BTreeSet::from([100, 200]));

    // Nothing new: nothing happens, and a restart does not replay the mailbox.
    assert_eq!(ingestor(&rig, &jobs).run_once().await.unwrap().messages, 0);
    let restarted = Arc::new(JmapMailSource::new(
        client(&rig.fake, PASSWORD).await,
        rig.cursors.clone(),
        rig.clock.clone(),
        Duration::from_hours(7 * 24),
        200,
    ));
    assert_eq!(restarted.fetch_new().await.unwrap().messages, Vec::new());
    assert_eq!(jobs.all_jobs().len(), 2);
}

#[tokio::test]
async fn a_new_chapter_after_the_download_finished_queues_the_work_again() {
    let rig = rig().await;
    let jobs = Arc::new(InMemoryJobs::new());
    rig.fake
        .deliver(MailSpec::ao3_chapter(100, 1, "2026-11-15T01:00:00Z"));
    ingestor(&rig, &jobs).run_once().await.unwrap();

    // The worker downloads it.
    let mut job = jobs.claim_next_for_tests(rig.clock.now()).await;
    job.succeed(rig.clock.now()).unwrap();
    jobs.save_for_tests(&job).await;

    // The author posts chapter 2; its notification arrives.
    rig.fake
        .deliver(MailSpec::ao3_chapter(100, 2, "2026-11-16T11:00:00Z"));
    let report = ingestor(&rig, &jobs).run_once().await.unwrap();
    assert_eq!(
        (report.enqueued, report.already_active),
        (1, 0),
        "{report:?}"
    );
    let all = jobs.all_jobs();
    assert_eq!(all.len(), 2);
    assert_eq!(
        all.iter().filter(|j| j.state == JobState::Queued).count(),
        1,
        "the update is queued; the old job stays done"
    );
}

#[tokio::test]
async fn a_second_notification_while_the_first_is_still_queued_adds_no_job() {
    let rig = rig().await;
    let jobs = Arc::new(InMemoryJobs::new());
    rig.fake
        .deliver(MailSpec::ao3_chapter(100, 1, "2026-11-15T01:00:00Z"));
    ingestor(&rig, &jobs).run_once().await.unwrap();
    rig.fake
        .deliver(MailSpec::ao3_chapter(100, 2, "2026-11-15T02:00:00Z"));
    let report = ingestor(&rig, &jobs).run_once().await.unwrap();
    assert_eq!((report.enqueued, report.already_active), (0, 1));
    assert_eq!(jobs.all_jobs().len(), 1);
}

/// Test-only helpers: `InMemoryJobs` has two `get`s and no claim shortcut.
trait JobsExt {
    async fn claim_next_for_tests(&self, now: Timestamp) -> fanfold_domain::job::Job;
    async fn save_for_tests(&self, job: &fanfold_domain::job::Job);
}

impl JobsExt for InMemoryJobs {
    async fn claim_next_for_tests(&self, now: Timestamp) -> fanfold_domain::job::Job {
        use fanfold_app::ports::JobRepository;
        self.claim_next(now).await.unwrap().expect("a job is due")
    }

    async fn save_for_tests(&self, job: &fanfold_domain::job::Job) {
        use fanfold_app::ports::JobRepository;
        self.save(job).await.unwrap();
    }
}

#[allow(dead_code)]
const _: &str = INBOX;

// ------------------------------------------------------------- the self-healing source

fn resilient(
    fake: &FakeJmap,
    cursors: &Arc<InMemoryCursors>,
    clock: &Arc<FakeClock>,
    password: &str,
) -> ResilientMailSource {
    ResilientMailSource::new(
        Arc::new(settings(fake, password)),
        cursors.clone(),
        clock.clone(),
        Duration::from_hours(7 * 24),
        200,
    )
}

fn sessions(fake: &FakeJmap) -> usize {
    fake.requests()
        .iter()
        .filter(|r| r.path == "/jmap/session")
        .count()
}

#[tokio::test]
async fn the_resilient_source_connects_on_demand_and_reuses_the_session() {
    let rig = rig().await;
    let source = resilient(&rig.fake, &rig.cursors, &rig.clock, PASSWORD);
    assert_eq!(
        sessions(&rig.fake),
        1,
        "only the rig's own connection so far: building it connects to nothing"
    );
    rig.fake
        .deliver(MailSpec::ao3_chapter(100, 1, "2026-11-10T01:00:00Z"));

    let first = source.fetch_new().await.unwrap();
    assert_eq!(first.messages.len(), 1);
    source.commit(&first.checkpoint).await.unwrap();
    source.fetch_new().await.unwrap();
    assert_eq!(sessions(&rig.fake), 2, "one session for all polls");
}

#[tokio::test]
async fn the_resilient_source_survives_the_server_being_down_at_start() {
    // Nothing is listening where the settings point: the first polls fail, but cleanly.
    let dead_base = {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        format!("http://{}", listener.local_addr().unwrap())
    };
    let cursors = Arc::new(InMemoryCursors::new());
    let clock = Arc::new(FakeClock::at_secs(
        NOW.parse::<Timestamp>().unwrap().as_second(),
    ));
    let source = ResilientMailSource::new(
        Arc::new(JmapSettings {
            base_url: dead_base.parse().unwrap(),
            username: USER.to_owned(),
            password: SecretString::from(PASSWORD.to_owned()),
            timeout: Duration::from_secs(2),
        }),
        cursors.clone(),
        clock,
        Duration::from_secs(3600),
        10,
    );
    for _ in 0..2 {
        let err = source.fetch_new().await.unwrap_err();
        assert!(err.to_string().contains("could not reach"), "{err}");
    }
    assert_eq!(cursors.get(CURSOR_KEY).await.unwrap(), None);
}

#[tokio::test]
async fn after_an_error_the_resilient_source_opens_a_fresh_session() {
    let rig = rig().await;
    let source = resilient(&rig.fake, &rig.cursors, &rig.clock, PASSWORD);
    source.fetch_new().await.unwrap();
    let before = sessions(&rig.fake);

    rig.fake.fail_api(Some(500));
    assert!(source.fetch_new().await.is_err());
    rig.fake.fail_api(None);

    rig.fake
        .deliver(MailSpec::ao3_chapter(7, 1, "2026-11-16T11:00:00Z"));
    assert_eq!(
        source.fetch_new().await.unwrap().messages.len(),
        1,
        "it recovers by itself"
    );
    assert_eq!(
        sessions(&rig.fake),
        before + 1,
        "on a new session, in case the old one went stale"
    );
}

#[tokio::test]
async fn committing_does_not_need_the_mail_server() {
    let rig = rig().await;
    let source = resilient(&rig.fake, &rig.cursors, &rig.clock, PASSWORD);
    let batch = rig.source.fetch_new().await.unwrap();
    rig.fake.fail_api(Some(500));
    source.commit(&batch.checkpoint).await.unwrap();
    assert_eq!(
        rig.cursors.get(CURSOR_KEY).await.unwrap().as_deref(),
        Some(batch.checkpoint.0.as_str())
    );
}

// ------------------------------------------------- error classification (mutation gaps)

#[tokio::test]
async fn rejected_credentials_are_recognised_however_the_server_says_so() {
    for problem_document in [false, true] {
        let fake = FakeJmap::start(USER, PASSWORD).await;
        fake.problem_json_errors(problem_document);
        let Err(err) = connect(&settings(&fake, "wrong"), Duration::from_secs(5)).await else {
            panic!("bad credentials must be rejected");
        };
        assert_eq!(err, JmapError::Auth, "problem document: {problem_document}");
    }
}

#[tokio::test]
async fn a_server_failure_reported_as_a_problem_document_is_not_mistaken_for_bad_credentials() {
    let rig = rig().await;
    rig.fake.problem_json_errors(true);
    rig.fake.fail_api(Some(500));
    let err = rig.source.fetch_new().await.unwrap_err().to_string();
    assert!(err.contains("reported an error"), "{err}");
    assert!(
        !err.contains("credentials"),
        "a 500 is not an authentication failure: {err}"
    );
}

#[tokio::test]
async fn a_method_failure_that_is_not_about_the_cursor_is_reported_not_mistaken_for_one() {
    let rig = rig().await;
    let first = rig.source.fetch_new().await.unwrap();
    rig.source.commit(&first.checkpoint).await.unwrap();
    rig.fake
        .deliver(MailSpec::ao3_chapter(100, 1, "2026-11-16T11:00:00Z"));
    let calls_before = rig.fake.api_methods().len();

    rig.fake.fail_method(Some(("Email/changes", "serverFail")));
    let err = rig.source.fetch_new().await.unwrap_err().to_string();
    assert!(err.contains("reported an error"), "{err}");
    assert_eq!(
        rig.fake.api_methods()[calls_before..],
        ["Email/changes"],
        "no backfill: that would hide a real failure behind a re-read of the mailbox"
    );
    assert_eq!(
        rig.cursors.get(CURSOR_KEY).await.unwrap().as_deref(),
        Some(first.checkpoint.0.as_str())
    );

    rig.fake.fail_method(None);
    assert_eq!(
        rig.source.fetch_new().await.unwrap().messages.len(),
        1,
        "and it recovers"
    );
}

#[tokio::test]
async fn bodies_are_requested_with_a_size_cap() {
    let rig = rig().await;
    rig.fake
        .deliver(MailSpec::ao3_chapter(100, 1, "2026-11-10T01:00:00Z"));
    rig.source.fetch_new().await.unwrap();
    let get = rig
        .fake
        .requests()
        .into_iter()
        .rfind(|r| r.method == "POST" && r.body.contains("fetchTextBodyValues"))
        .expect("an Email/get with bodies");
    let body: serde_json::Value = serde_json::from_str(&get.body).unwrap();
    let args = &body["methodCalls"][0][1];
    assert_eq!(args["maxBodyValueBytes"], 256 * 1024, "{args}");
    assert_eq!(
        (
            args["fetchTextBodyValues"].clone(),
            args["fetchHTMLBodyValues"].clone()
        ),
        (true.into(), true.into())
    );
}
