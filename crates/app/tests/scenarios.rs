//! End-to-end scenarios for the use cases, wired to in-memory adapters.
//!
//! These read as the behaviour we promise: what happens on an outage, an update,
//! a crash, a replayed mailbox.

// Integration-test helpers are not covered by clippy's `allow-*-in-tests`; a failed
// expectation here should simply panic.
// `Arc::clone(&x)` would block the `Arc<T>` -> `Arc<dyn Port>` coercion in the harness.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::clone_on_ref_ptr
)]

use std::{sync::Arc, time::Duration};

use fanfold_app::{
    error::{AppError, RepoError},
    ingest::{IngestConfig, Ingestor},
    ports::{
        Checkpoint, JobRepository, LogLevel, MailBatch, MailMessage, MetadataStore as _,
        NotifyEvent,
    },
    worker::{Outcome, Ports, Tick, Worker, WorkerConfig},
};
use fanfold_domain::{
    breaker::BreakerConfig,
    failure::FailureKind,
    job::{JobId, JobSource, JobState, Resolution, Target},
    metadata::{FollowSource, SeriesPart, Tag, TagKind},
    retry::RetryPolicy,
    work::{SeriesId, WorkId},
};
use fanfold_testkit::memory::{
    FakeClock, FixedJitter, InMemoryJobs, InMemoryLibrary, InMemoryMail, RecordingNotifier,
    ScriptedDownloader,
};

const START: i64 = 1_000;

struct Harness {
    jobs: Arc<InMemoryJobs>,
    library: Arc<InMemoryLibrary>,
    downloader: Arc<ScriptedDownloader>,
    notifier: Arc<RecordingNotifier>,
    clock: Arc<FakeClock>,
    worker: Worker,
}

impl Harness {
    fn new() -> Self {
        let jobs = Arc::new(InMemoryJobs::new());
        let library = Arc::new(InMemoryLibrary::new());
        let downloader = Arc::new(ScriptedDownloader::new());
        let notifier = Arc::new(RecordingNotifier::new());
        let clock = Arc::new(FakeClock::at_secs(START));
        let worker = Worker::new(
            Ports {
                jobs: jobs.clone(),
                catalog: jobs.clone(),
                metadata: jobs.clone(),
                downloader: downloader.clone(),
                library: library.clone(),
                notifier: notifier.clone(),
                clock: clock.clone(),
                jitter: Arc::new(FixedJitter(0.5)),
            },
            WorkerConfig {
                retry: RetryPolicy {
                    max_attempts: 3,
                    base: Duration::from_secs(60),
                    cap: Duration::from_secs(3600),
                },
                breaker: BreakerConfig {
                    failure_threshold: 3,
                    open_for: Duration::from_secs(60),
                    max_open_for: Duration::from_secs(600),
                },
            },
        );
        Self {
            jobs,
            library,
            downloader,
            notifier,
            clock,
            worker,
        }
    }

    async fn enqueue(&self, work: u64) -> JobId {
        let outcome = self
            .jobs
            .enqueue(
                Target::Work(WorkId::new(work).unwrap()),
                JobSource::Manual,
                self.clock_now(),
            )
            .await
            .unwrap();
        match outcome {
            fanfold_app::ports::EnqueueOutcome::Created(job) => job.id,
            fanfold_app::ports::EnqueueOutcome::AlreadyActive(id) => id,
        }
    }

    fn clock_now(&self) -> fanfold_domain::Timestamp {
        use fanfold_app::ports::Clock as _;
        self.clock.now()
    }

    async fn job(&self, id: JobId) -> fanfold_domain::job::Job {
        self.jobs.get(id).await.unwrap().unwrap()
    }
}

const fn retrying(tick: &Tick) -> bool {
    matches!(
        tick,
        Tick::Processed {
            outcome: Outcome::Failed(Resolution::Retrying { .. }),
            ..
        }
    )
}

#[tokio::test]
async fn first_download_is_published_under_a_derived_name() {
    let h = Harness::new();
    let id = h.enqueue(5).await;
    h.downloader.push_ok("My Story");

    let tick = h.worker.run_once().await.unwrap();

    assert_eq!(
        tick,
        Tick::Processed {
            job: id,
            outcome: Outcome::Succeeded
        }
    );
    assert_eq!(h.job(id).await.state, JobState::Succeeded);
    assert_eq!(h.library.published()[0].0, "single/Author/My Story 5.epub");
    assert_eq!(h.downloader.requests()[0].existing, None);
    let events = h.notifier.events();
    let [
        NotifyEvent::Downloaded {
            job,
            work,
            title,
            updated: false,
            details: Some(details),
        },
    ] = events.as_slice()
    else {
        panic!("{events:?}")
    };
    assert_eq!(
        (*job, *work, title.as_str()),
        (id, WorkId::new(5).unwrap(), "My Story")
    );
    assert_eq!(details.path, "single/Author/My Story 5.epub");
    assert_eq!(details.metadata.facts.title, "My Story");
    assert!(details.previous.is_none() && details.new_chapters.is_empty());
    assert_eq!(h.worker.run_once().await.unwrap(), Tick::Idle);
}

#[tokio::test]
async fn updates_keep_the_original_file_name_even_if_the_title_changes() {
    let h = Harness::new();
    h.enqueue(5).await;
    h.downloader.push_ok("My Story");
    h.worker.run_once().await.unwrap();

    h.clock.advance(Duration::from_secs(3600));
    h.enqueue(5).await; // the author posted a new chapter
    h.downloader.push_ok("My Story (Renamed!)");
    h.worker.run_once().await.unwrap();

    let published = h.library.published();
    assert_eq!(published.len(), 2);
    assert_eq!(
        published[1].0, "single/Author/My Story 5.epub",
        "path must not move: readers key progress on it"
    );
    assert_eq!(
        h.downloader.requests()[1]
            .existing
            .as_deref()
            .and_then(|p| p.to_str()),
        Some("/library/single/Author/My Story 5.epub"),
        "the existing file is handed over for an incremental update"
    );
    assert!(matches!(
        h.notifier.events().last(),
        Some(NotifyEvent::Downloaded { updated: true, .. })
    ));
}

#[tokio::test]
async fn a_book_deleted_from_the_library_is_downloaded_afresh_under_its_recorded_name() {
    let h = Harness::new();
    h.enqueue(5).await;
    h.downloader.push_ok("My Story");
    h.worker.run_once().await.unwrap();

    // The catalog still remembers the book, but the file is gone.
    h.library.forget("single/Author/My Story 5.epub");
    h.clock.advance(Duration::from_secs(3600));
    h.enqueue(5).await;
    h.downloader.push_ok("My Story");
    h.worker.run_once().await.unwrap();

    assert_eq!(
        h.downloader.requests()[1].existing,
        None,
        "nothing to update incrementally"
    );
    assert_eq!(
        h.library.published()[1].0,
        "single/Author/My Story 5.epub",
        "same path, so the reader keeps its place"
    );
    assert!(matches!(
        h.notifier.events().last(),
        Some(NotifyEvent::Downloaded { updated: false, .. })
    ));
}

/// A book that was downloaded wrong (e.g. with no chapters) cannot be fixed by an update, which
/// only adds to it: a forced retry downloads from scratch and replaces it under the same name.
#[tokio::test]
async fn a_forced_retry_replaces_the_stored_book_instead_of_updating_it() {
    let h = Harness::new();
    let admin = fanfold_app::admin::JobAdmin::new(h.jobs.clone(), h.clock.clone());
    let id = h.enqueue(5).await;
    h.downloader.push_ok("My Story");
    h.worker.run_once().await.unwrap();

    h.clock.advance(Duration::from_secs(60));
    admin.force_retry(id).await.unwrap();
    // The first forced run fails: the next one must still be forced.
    h.downloader
        .push_err(FailureKind::SiteUnavailable, "HTTP 525");
    h.worker.run_once().await.unwrap();
    assert!(h.job(id).await.force);
    h.clock.advance(Duration::from_secs(3600));
    h.downloader.push_ok("My Story");
    h.worker.run_once().await.unwrap();

    for request in &h.downloader.requests()[1..] {
        assert_eq!(request.existing, None, "the stored book is not updated");
    }
    assert_eq!(h.library.published()[1].0, "single/Author/My Story 5.epub");
    let job = h.job(id).await;
    assert_eq!((job.state, job.force), (JobState::Succeeded, false));
    let logs = h.jobs.logs(id).await.unwrap();
    assert!(
        logs.iter()
            .filter(|l| l.message == fanfold_app::worker::FORCED_NOTE)
            .count()
            == 2,
        "each forced run says so"
    );
    assert!(matches!(
        h.notifier.events().last(),
        Some(NotifyEvent::Downloaded { updated: true, .. })
    ));
}

#[tokio::test]
async fn an_update_with_nothing_new_touches_nothing_and_stays_quiet() {
    let h = Harness::new();
    h.enqueue(5).await;
    h.downloader.push_ok("My Story");
    h.worker.run_once().await.unwrap();
    let events_before = h.notifier.events().len();

    h.clock.advance(Duration::from_secs(3600));
    let id = h.enqueue(5).await;
    h.downloader.push_unchanged("My Story");
    let tick = h.worker.run_once().await.unwrap();

    assert_eq!(
        tick,
        Tick::Processed {
            job: id,
            outcome: Outcome::Succeeded
        }
    );
    assert_eq!(h.library.published().len(), 1, "nothing was re-published");
    assert_eq!(
        h.notifier.events().len(),
        events_before,
        "and nobody was pinged"
    );
    let logs = h.jobs.logs(id).await.unwrap();
    assert!(
        logs.iter()
            .any(|l| l.message.contains("already up to date"))
    );
}

#[tokio::test]
async fn an_outage_trips_the_breaker_without_burning_any_retry_budget() {
    let h = Harness::new();
    let ids = [
        h.enqueue(1).await,
        h.enqueue(2).await,
        h.enqueue(3).await,
        h.enqueue(4).await,
    ];
    for _ in 0..3 {
        h.downloader
            .push_err(FailureKind::SiteUnavailable, "HTTP 525");
    }

    for _ in 0..3 {
        assert!(retrying(&h.worker.run_once().await.unwrap()));
    }
    let events = h.notifier.events();
    let [
        NotifyEvent::SiteDegraded {
            kind: FailureKind::SiteUnavailable,
            message,
            retry_at,
            waiting: Some(waiting),
        },
    ] = events.as_slice()
    else {
        panic!("announced exactly once: {events:?}")
    };
    assert_eq!(message, "HTTP 525");
    assert!(*retry_at > h.clock_now(), "the next probe is in the future");
    assert_eq!(*waiting, 4, "every queued job waits");
    assert_eq!(
        h.worker.run_once().await.unwrap(),
        Tick::BreakerOpen,
        "the queue is not drained"
    );
    assert_eq!(
        h.downloader.requests().len(),
        3,
        "no request was made while open"
    );
    for id in &ids[..3] {
        let job = h.job(*id).await;
        assert_eq!(
            (job.state, job.attempt),
            (JobState::Queued, 0),
            "budget untouched"
        );
    }

    // Cool-down passes: one probe goes out, succeeds, the breaker closes.
    h.clock.advance(Duration::from_secs(60));
    h.downloader.push_ok("Back Online");
    h.downloader.push_ok("Second");
    let probe = h.worker.run_once().await.unwrap();
    assert!(matches!(
        probe,
        Tick::Processed {
            outcome: Outcome::Succeeded,
            ..
        }
    ));
    let Some(NotifyEvent::SiteRecovered { down_since, at }) = h.notifier.events().last().cloned()
    else {
        panic!("recovery announced")
    };
    assert!(
        down_since.is_some_and(|since| since < at),
        "the outage has a start"
    );
    assert!(matches!(
        h.worker.run_once().await.unwrap(),
        Tick::Processed {
            outcome: Outcome::Succeeded,
            ..
        }
    ));
}

#[tokio::test]
async fn a_failed_probe_keeps_the_breaker_open_for_longer() {
    let h = Harness::new();
    for n in 1..=4 {
        h.enqueue(n).await;
    }
    // Three failures trip the breaker, then two failed probes.
    for _ in 0..5 {
        h.downloader.push_err(FailureKind::RateLimited, "429");
    }
    for _ in 0..3 {
        h.worker.run_once().await.unwrap();
    }
    h.clock.advance(Duration::from_secs(60));
    assert!(
        retrying(&h.worker.run_once().await.unwrap()),
        "the probe runs and fails"
    );
    h.clock.advance(Duration::from_secs(60));
    assert_eq!(
        h.worker.run_once().await.unwrap(),
        Tick::BreakerOpen,
        "cool-down doubled to 120s"
    );
    h.clock.advance(Duration::from_secs(60));
    assert!(retrying(&h.worker.run_once().await.unwrap()));
    assert_eq!(
        h.notifier.events().len(),
        1,
        "no repeated degraded announcements"
    );
}

#[tokio::test]
async fn missing_works_fail_fast_and_never_trip_the_breaker() {
    let h = Harness::new();
    for n in 1..=6 {
        h.enqueue(n).await;
        h.downloader
            .push_err(FailureKind::NotFound, "deleted by author");
    }
    for _ in 0..6 {
        let tick = h.worker.run_once().await.unwrap();
        assert!(matches!(
            tick,
            Tick::Processed {
                outcome: Outcome::Failed(Resolution::GaveUp),
                ..
            }
        ));
    }
    let events = h.notifier.events();
    assert_eq!(events.len(), 6);
    assert!(events.iter().all(|e| matches!(
        e,
        NotifyEvent::GaveUp {
            kind: FailureKind::NotFound,
            ..
        }
    )));
}

#[tokio::test]
async fn ordinary_failures_spend_the_budget_then_give_up() {
    let h = Harness::new();
    let id = h.enqueue(1).await;
    for _ in 0..3 {
        h.downloader.push_err(FailureKind::Transient, "parse error");
    }
    let mut ticks = Vec::new();
    for _ in 0..3 {
        ticks.push(h.worker.run_once().await.unwrap());
        h.clock.advance(Duration::from_secs(3600));
    }
    assert!(retrying(&ticks[0]) && retrying(&ticks[1]));
    assert!(matches!(
        ticks[2],
        Tick::Processed {
            outcome: Outcome::Failed(Resolution::GaveUp),
            ..
        }
    ));
    let job = h.job(id).await;
    assert_eq!((job.state, job.attempt), (JobState::Failed, 3));
    assert_eq!(job.last_error.as_deref(), Some("parse error"));
    assert_eq!(h.worker.run_once().await.unwrap(), Tick::Idle);
}

#[tokio::test]
async fn retry_waits_for_the_back_off() {
    let h = Harness::new();
    h.enqueue(1).await;
    h.downloader.push_err(FailureKind::Transient, "oops");
    h.worker.run_once().await.unwrap();
    assert_eq!(
        h.worker.run_once().await.unwrap(),
        Tick::Idle,
        "not due yet"
    );
    h.clock.advance(Duration::from_secs(59));
    assert_eq!(h.worker.run_once().await.unwrap(), Tick::Idle);
    h.clock.advance(Duration::from_secs(1));
    h.downloader.push_ok("Finally");
    assert!(matches!(
        h.worker.run_once().await.unwrap(),
        Tick::Processed {
            outcome: Outcome::Succeeded,
            ..
        }
    ));
}

#[tokio::test]
async fn scratch_space_is_released_after_every_returned_book() {
    let h = Harness::new();
    // 1. new book, 2. update with changes, 3. update with nothing new, 4. publish fails.
    h.enqueue(1).await;
    h.downloader.push_ok("A");
    h.worker.run_once().await.unwrap();
    assert_eq!(
        h.downloader.released().len(),
        1,
        "after a successful publish"
    );

    h.enqueue(1).await;
    h.downloader.push_unchanged("A");
    h.worker.run_once().await.unwrap();
    assert_eq!(
        h.downloader.released().len(),
        2,
        "after an update that found nothing"
    );

    h.enqueue(2).await;
    h.library.fail_publish(true);
    h.downloader.push_ok("B");
    h.worker.run_once().await.unwrap();
    assert_eq!(
        h.downloader.released().len(),
        3,
        "even when publishing failed"
    );

    // A failed download returns no book, so there is nothing to release.
    h.enqueue(3).await;
    h.downloader.push_err(FailureKind::Transient, "boom");
    h.worker.run_once().await.unwrap();
    assert_eq!(h.downloader.released().len(), 3);
}

#[tokio::test]
async fn a_full_disk_is_retried_not_lost() {
    let h = Harness::new();
    let id = h.enqueue(1).await;
    h.library.fail_publish(true);
    h.downloader.push_ok("Story");
    assert!(retrying(&h.worker.run_once().await.unwrap()));
    let job = h.job(id).await;
    assert!(job.last_error.unwrap().contains("disk full"));
    assert_eq!(h.library.published(), Vec::new());
}

#[tokio::test]
async fn a_notification_outage_never_fails_a_download() {
    let h = Harness::new();
    let id = h.enqueue(1).await;
    h.notifier.fail(true);
    h.downloader.push_ok("Story");
    assert!(matches!(
        h.worker.run_once().await.unwrap(),
        Tick::Processed {
            outcome: Outcome::Succeeded,
            ..
        }
    ));
    assert_eq!(h.job(id).await.state, JobState::Succeeded);
}

#[tokio::test]
async fn the_job_log_explains_what_happened() {
    let h = Harness::new();
    let id = h.enqueue(1).await;
    h.downloader
        .push_err(FailureKind::SiteUnavailable, "HTTP 503");
    h.worker.run_once().await.unwrap();

    let logs = h.jobs.logs(id).await.unwrap();
    let messages: Vec<&str> = logs.iter().map(|l| l.message.as_str()).collect();
    assert_eq!(messages[0], "starting attempt 1");
    assert!(
        messages
            .iter()
            .any(|m| m.contains("worker output for: HTTP 503")),
        "{messages:?}"
    );
    let last = logs.last().unwrap();
    assert_eq!(last.level, LogLevel::Error);
    assert!(
        last.message.contains("site_unavailable") && last.message.contains("retrying at"),
        "{}",
        last.message
    );
}

#[tokio::test]
async fn jobs_orphaned_by_a_crash_are_recovered() {
    let h = Harness::new();
    let id = h.enqueue(1).await;
    // A previous process claimed it and died.
    let claimed = h.jobs.claim_next(h.clock_now()).await.unwrap().unwrap();
    assert_eq!(claimed.id, id);
    assert_eq!(
        h.worker.run_once().await.unwrap(),
        Tick::Idle,
        "invisible while 'running'"
    );

    assert_eq!(h.worker.recover().await.unwrap(), 1);
    h.downloader.push_ok("Story");
    assert!(matches!(
        h.worker.run_once().await.unwrap(),
        Tick::Processed {
            outcome: Outcome::Succeeded,
            ..
        }
    ));
}

#[tokio::test]
async fn a_database_outage_surfaces_as_an_error_not_a_silent_skip() {
    let h = Harness::new();
    h.jobs
        .break_with(Some(RepoError::Backend("connection reset".into())));
    assert!(matches!(h.worker.run_once().await, Err(AppError::Repo(_))));
    h.jobs.break_with(None);
    assert_eq!(h.worker.run_once().await.unwrap(), Tick::Idle);
}

// ----------------------------------------------------------------------- mail

fn mail(id: &str, from: &str, body: &str) -> MailMessage {
    MailMessage {
        id: id.into(),
        from: from.into(),
        subject: "update".into(),
        text: body.into(),
        html: String::new(),
    }
}

fn ingestor(h: &Harness, mail: Arc<InMemoryMail>) -> Ingestor {
    Ingestor::new(
        mail,
        h.jobs.clone(),
        h.jobs.clone(),
        h.clock.clone(),
        IngestConfig {
            allowed_senders: vec!["archiveofourown.org".into()],
            max_links_per_message: 50,
        },
    )
}

#[tokio::test]
async fn a_replayed_mailbox_does_not_create_duplicate_downloads() {
    let h = Harness::new();
    let mailbox = Arc::new(InMemoryMail::new());
    // After four weeks of downtime AO3 releases its backlog: many emails, same work.
    mailbox.push(MailBatch {
        messages: vec![
            mail(
                "1",
                "AO3 <x@archiveofourown.org>",
                "https://archiveofourown.org/works/7/chapters/1",
            ),
            mail(
                "2",
                "AO3 <x@archiveofourown.org>",
                "https://archiveofourown.org/works/7/chapters/2",
            ),
            mail(
                "3",
                "AO3 <x@archiveofourown.org>",
                "https://archiveofourown.org/works/8",
            ),
        ],
        checkpoint: Checkpoint("cp1".into()),
    });
    let ingest = ingestor(&h, mailbox.clone());

    // The database is down: nothing may be committed, so the batch is replayed.
    h.jobs.break_with(Some(RepoError::Backend("down".into())));
    assert!(ingest.run_once().await.is_err());
    assert_eq!(mailbox.pending_batches(), 1);
    assert_eq!(mailbox.committed(), Vec::new());

    h.jobs.break_with(None);
    let report = ingest.run_once().await.unwrap();
    assert_eq!((report.enqueued, report.already_active), (2, 1));
    assert_eq!(mailbox.pending_batches(), 0);
    assert_eq!(h.jobs.all_jobs().len(), 2);

    // Delivering the very same batch again (e.g. crash after enqueue, before commit)
    // is harmless.
    mailbox.push(MailBatch {
        messages: vec![mail(
            "1",
            "AO3 <x@archiveofourown.org>",
            "https://archiveofourown.org/works/7",
        )],
        checkpoint: Checkpoint("cp1-again".into()),
    });
    let report = ingest.run_once().await.unwrap();
    assert_eq!((report.enqueued, report.already_active), (0, 1));
    assert_eq!(h.jobs.all_jobs().len(), 2);
}

#[tokio::test]
async fn mail_to_epub_end_to_end() {
    let h = Harness::new();
    let mailbox = Arc::new(InMemoryMail::new());
    mailbox.push(MailBatch {
        messages: vec![mail(
            "1",
            "AO3 <x@archiveofourown.org>",
            "New chapter: https://archiveofourown.org/works/31337",
        )],
        checkpoint: Checkpoint("cp".into()),
    });
    ingestor(&h, mailbox).run_once().await.unwrap();
    h.downloader.push_ok("The Long Fic");
    h.worker.run_once().await.unwrap();
    assert_eq!(
        h.library.published()[0].0,
        "single/Author/The Long Fic 31337.epub"
    );
}

// ------------------------------------------------------- site status and stats

#[tokio::test]
async fn site_status_follows_the_breaker() {
    use fanfold_app::ports::{SiteStatus, SiteStatusSource};
    let h = Harness::new();
    assert_eq!(h.worker.site_status(), SiteStatus::Healthy);
    for n in 1..=3 {
        h.enqueue(n).await;
        h.downloader.push_err(FailureKind::SiteUnavailable, "525");
        h.worker.run_once().await.unwrap();
    }
    // Tripped on the third failure at START; cool-down is 60 s.
    let SiteStatus::Degraded { retry_at } = h.worker.site_status() else {
        panic!("expected Degraded, got {:?}", h.worker.site_status());
    };
    assert_eq!(
        retry_at,
        fanfold_domain::timestamp::add(h.clock_now(), Duration::from_secs(60))
    );

    h.clock.advance(Duration::from_secs(60));
    h.downloader.push_ok("Back");
    h.worker.run_once().await.unwrap();
    assert_eq!(h.worker.site_status(), SiteStatus::Healthy);
}

#[tokio::test]
async fn worker_stats_count_what_happened() {
    let h = Harness::new();
    h.enqueue(1).await;
    h.downloader.push_ok("A");
    h.worker.run_once().await.unwrap();
    h.clock.advance(Duration::from_secs(60));
    h.enqueue(1).await;
    h.downloader.push_unchanged("A");
    h.worker.run_once().await.unwrap();
    h.enqueue(2).await;
    h.downloader.push_err(FailureKind::NotFound, "gone");
    h.worker.run_once().await.unwrap();
    for n in 3..=5 {
        h.enqueue(n).await;
        h.downloader.push_err(FailureKind::SiteUnavailable, "525");
        h.worker.run_once().await.unwrap();
    }

    let s = h.worker.stats().snapshot();
    assert_eq!((s.downloaded, s.updated, s.unchanged), (1, 0, 1));
    assert_eq!((s.gave_up, s.retry_scheduled), (1, 3));
    assert_eq!(s.breaker_trips, 1);
    let failures = |kind| s.failures.iter().find(|(k, _)| *k == kind).unwrap().1;
    assert_eq!(failures(FailureKind::SiteUnavailable), 3);
    assert_eq!(failures(FailureKind::NotFound), 1);
    assert!(s.last_success.is_some());
    assert_eq!(s.duration.count, 6);
}

// ---------------------------------------------------------------------- series

fn part(series: u64, name: &str, part: u32) -> SeriesPart {
    SeriesPart {
        id: SeriesId::new(series).unwrap(),
        name: name.into(),
        part: Some(part),
    }
}

async fn queue_series(h: &Harness, series: u64) -> JobId {
    match h
        .jobs
        .enqueue(
            Target::Series(SeriesId::new(series).unwrap()),
            JobSource::Manual,
            h.clock_now(),
        )
        .await
        .unwrap()
    {
        fanfold_app::ports::EnqueueOutcome::Created(job) => job.id,
        fanfold_app::ports::EnqueueOutcome::AlreadyActive(id) => id,
    }
}

#[tokio::test]
async fn adding_a_series_lists_it_follows_it_and_files_every_work_under_it() {
    let h = Harness::new();
    let job = queue_series(&h, 2_000_001).await;
    h.downloader
        .push_listing("The Harbor Saga", &[10_000_002, 10_000_004]);
    h.worker.run_once().await.unwrap();

    assert_eq!(h.job(job).await.state, JobState::Succeeded);
    let series = SeriesId::new(2_000_001).unwrap();
    let history = h.jobs.series_history(series);
    assert_eq!(
        history.followed.map(|(by, _)| by),
        Some(FollowSource::Manual)
    );
    assert_eq!(history.folder.as_deref(), Some("The Harbor Saga 2000001"));
    assert_eq!(history.versions.len(), 1);
    let logs = h.jobs.logs(job).await.unwrap();
    assert!(
        logs.iter().any(
            |l| l.message == "listed \"The Harbor Saga\": 2 works; queued 2, 0 already queued"
        ),
        "{logs:?}"
    );

    // The works it queued are downloaded in series order, each into the series folder.
    h.downloader
        .push_ok_with("The Lighthouse Keeper", |m| {
            m.series = vec![part(2_000_001, "The Harbor Saga", 1)];
        });
    h.downloader
        .push_ok_with("The Keeper and the Logbook", |m| {
            // Renamed since the listing: the folder chosen first stays.
            m.series = vec![part(2_000_001, "The Harbor Saga (renamed)", 2)];
        });
    h.worker.run_once().await.unwrap();
    h.worker.run_once().await.unwrap();
    let published: Vec<String> = h.library.published().into_iter().map(|(p, _)| p).collect();
    assert_eq!(
        published,
        [
            "series/The Harbor Saga 2000001/01 The Lighthouse Keeper 10000002.epub",
            "series/The Harbor Saga 2000001/02 The Keeper and the Logbook 10000004.epub",
        ]
    );
}

#[tokio::test]
async fn a_series_page_that_lists_nothing_is_a_failure_not_an_empty_series() {
    let h = Harness::new();
    let job = queue_series(&h, 7).await;
    h.downloader.push_listing("Broken", &[]);
    let tick = h.worker.run_once().await.unwrap();
    assert!(retrying(&tick), "{tick:?}");
    assert!(
        h.jobs
            .series_history(SeriesId::new(7).unwrap())
            .followed
            .is_none()
    );
    assert!(
        h.job(job)
            .await
            .last_error
            .unwrap()
            .contains("listed no works")
    );

    h.clock.advance(Duration::from_secs(3600));
    h.downloader
        .push_listing_err(FailureKind::SiteUnavailable, "HTTP 525");
    assert!(retrying(&h.worker.run_once().await.unwrap()));
}

#[tokio::test]
async fn a_series_email_follows_the_series_and_fetches_only_the_works_it_links() {
    let h = Harness::new();
    let mailbox = Arc::new(InMemoryMail::new());
    mailbox.push(MailBatch {
        messages: vec![mail(
            "1",
            "AO3 <x@archiveofourown.org>",
            "quillandink posted Part 4 of https://archiveofourown.org/series/2000001 : \
             https://archiveofourown.org/works/10000007 \
             and an unrelated work https://archiveofourown.org/works/111",
        )],
        checkpoint: Checkpoint("cp".into()),
    });
    let report = ingestor(&h, mailbox).run_once().await.unwrap();
    assert_eq!((report.series_followed, report.enqueued), (1, 2));
    assert!(
        h.downloader.listed().is_empty(),
        "the series is not listed again"
    );
    let series = SeriesId::new(2_000_001).unwrap();
    assert_eq!(
        h.jobs.series_history(series).followed.map(|(by, _)| by),
        Some(FollowSource::Mail)
    );

    // Which work is in the series comes from each work's own page, not the email layout.
    h.downloader.push_ok_with("Part Four", |m| {
        m.series = vec![part(2_000_001, "The Harbor Saga", 4)];
    });
    h.downloader.push_ok("Unrelated");
    h.worker.run_once().await.unwrap();
    h.worker.run_once().await.unwrap();
    let published: Vec<String> = h.library.published().into_iter().map(|(p, _)| p).collect();
    assert_eq!(
        published,
        [
            "series/The Harbor Saga 2000001/04 Part Four 10000007.epub",
            "single/Author/Unrelated 111.epub",
        ]
    );
}

#[tokio::test]
async fn a_work_in_several_series_goes_under_the_first_followed_one() {
    let h = Harness::new();
    h.jobs
        .follow_series(
            SeriesId::new(2).unwrap(),
            FollowSource::Manual,
            h.clock_now(),
        )
        .await
        .unwrap();
    h.enqueue(9).await;
    h.downloader.push_ok_with("Crossover", |m| {
        m.series = vec![part(1, "Not Followed", 3), part(2, "Followed", 5)];
    });
    h.worker.run_once().await.unwrap();
    assert_eq!(
        h.library.published()[0].0,
        "series/Followed 2/05 Crossover 9.epub"
    );
}

#[tokio::test]
async fn a_book_keeps_its_path_when_its_series_is_followed_later() {
    let h = Harness::new();
    h.enqueue(9).await;
    h.downloader.push_ok_with("Early", |m| {
        m.series = vec![part(2, "Later Followed", 1)];
    });
    h.worker.run_once().await.unwrap();
    h.jobs
        .follow_series(
            SeriesId::new(2).unwrap(),
            FollowSource::Manual,
            h.clock_now(),
        )
        .await
        .unwrap();
    h.clock.advance(Duration::from_secs(60));
    h.enqueue(9).await;
    h.downloader.push_ok_with("Early", |m| {
        m.series = vec![part(2, "Later Followed", 1)];
    });
    h.worker.run_once().await.unwrap();
    let published: Vec<String> = h.library.published().into_iter().map(|(p, _)| p).collect();
    assert_eq!(
        published,
        ["single/Author/Early 9.epub", "single/Author/Early 9.epub"]
    );
}

// --------------------------------------------------------------------- history

#[tokio::test]
async fn every_download_records_the_metadata_and_only_changes_make_versions() {
    let h = Harness::new();
    let work = WorkId::new(5).unwrap();
    let id = h.enqueue(5).await;
    h.downloader.push_ok_with("Story", |m| {
        m.stats.hits = Some(10);
    });
    h.worker.run_once().await.unwrap();

    // Same details, more hits: statistics only.
    h.clock.advance(Duration::from_secs(3600));
    h.enqueue(5).await;
    h.downloader.push_ok_with("Story", |m| {
        m.stats.hits = Some(20);
    });
    h.worker.run_once().await.unwrap();
    let history = h.jobs.work_history(work);
    assert_eq!(history.versions.len(), 1);
    assert_eq!(history.stats.len(), 2);

    // A new chapter and a new tag: one new version, the tag's span opens in it.
    h.clock.advance(Duration::from_secs(3600));
    let second = h.enqueue(5).await;
    h.downloader.push_ok_with("Story", |m| {
        m.facts.chapters_posted = 2;
        m.tags.push(Tag {
            kind: TagKind::Freeform,
            name: "Fluff".into(),
        });
    });
    h.worker.run_once().await.unwrap();
    let history = h.jobs.work_history(work);
    assert_eq!(history.versions.len(), 2);
    let added = history
        .tags
        .iter()
        .find(|s| s.item.name == "Fluff")
        .unwrap();
    assert_eq!(added.added_in, history.versions[1].id);
    assert_eq!(history.versions[1].job, Some(second));
    assert_eq!(history.versions[0].job, Some(id));
    assert_eq!(history.versions[1].facts.chapters_posted, 2);
    let logs = h.jobs.logs(second).await.unwrap();
    assert!(
        logs.iter()
            .any(|l| l.message == fanfold_app::worker::METADATA_CHANGED_NOTE)
    );
}

#[tokio::test]
async fn a_failed_metadata_store_fails_the_run_and_publishes_nothing() {
    let h = Harness::new();
    let id = h.enqueue(5).await;
    h.downloader.push_ok("Story");
    h.jobs
        .break_metadata_with(Some(RepoError::Backend("db down".into())));
    let tick = h.worker.run_once().await.unwrap();
    assert!(retrying(&tick), "{tick:?}");
    assert!(
        h.library.published().is_empty(),
        "no book without its history"
    );
    assert_eq!(
        h.downloader.released().len(),
        1,
        "scratch space is still freed"
    );
    assert!(h.job(id).await.last_error.unwrap().contains("metadata"));
}

// --------------------------------------------------------------- notifications

#[tokio::test]
async fn an_update_reports_exactly_the_chapters_it_added_and_what_else_changed() {
    use fanfold_domain::metadata::Chapter;
    let chapter = |n: u32| Chapter {
        number: n,
        title: format!("Chapter {n}"),
        ao3_id: Some(u64::from(n) * 100),
        posted_on: None,
    };
    let h = Harness::new();
    h.enqueue(5).await;
    h.downloader.push_ok_with("Story", |m| {
        m.chapters = vec![chapter(1), chapter(2)];
        m.facts.chapters_posted = 2;
    });
    h.worker.run_once().await.unwrap();

    h.clock.advance(Duration::from_secs(3600));
    let update = h.enqueue(5).await;
    h.downloader.push_ok_with("Story", |m| {
        // Chapter 2 was renamed in place (same AO3 id): not new.
        let mut renamed = chapter(2);
        renamed.title = "Chapter 2 (edited)".into();
        m.chapters = vec![chapter(1), renamed, chapter(3), chapter(4)];
        m.facts.chapters_posted = 4;
        m.facts.status = "Completed".into();
    });
    h.worker.run_once().await.unwrap();

    let Some(NotifyEvent::Downloaded {
        job,
        updated: true,
        details: Some(details),
        ..
    }) = h.notifier.events().last().cloned()
    else {
        panic!("{:?}", h.notifier.events())
    };
    assert_eq!(job, update);
    let added: Vec<u32> = details.new_chapters.iter().map(|c| c.number).collect();
    assert_eq!(added, [3, 4]);
    let before = details.previous.expect("the previous version is known");
    assert_eq!(before.facts.chapters_posted, 2);
    assert_eq!(details.metadata.facts.status, "Completed");
}

#[tokio::test]
async fn giving_up_names_the_work_and_the_budget() {
    let h = Harness::new();
    let id = h.enqueue(5).await;
    h.downloader.push_ok("Known Title");
    h.worker.run_once().await.unwrap();
    h.clock.advance(Duration::from_secs(60));
    let again = h.enqueue(5).await;
    assert_ne!(id, again);
    h.downloader
        .push_err(FailureKind::NotFound, "deleted by its author");
    h.worker.run_once().await.unwrap();
    let Some(NotifyEvent::GaveUp {
        job,
        title,
        kind,
        attempts,
        max_attempts,
        ..
    }) = h.notifier.events().last().cloned()
    else {
        panic!("{:?}", h.notifier.events())
    };
    assert_eq!(job, again);
    assert_eq!(title.as_deref(), Some("Known Title"));
    assert_eq!(kind, FailureKind::NotFound);
    assert_eq!((attempts, max_attempts), (1, 3));
}

#[tokio::test]
async fn listing_a_series_is_announced() {
    let h = Harness::new();
    let job = queue_series(&h, 77).await;
    h.downloader.push_listing("A Saga", &[1, 2, 3]);
    h.worker.run_once().await.unwrap();
    assert_eq!(
        h.notifier.events(),
        [NotifyEvent::SeriesListed {
            job,
            series: SeriesId::new(77).unwrap(),
            name: "A Saga".into(),
            works: 3,
            queued: 3,
        }]
    );
}
