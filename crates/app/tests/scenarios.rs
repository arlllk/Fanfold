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
    ports::{Checkpoint, JobRepository, LogLevel, MailBatch, MailMessage, NotifyEvent},
    worker::{Outcome, Ports, Tick, Worker, WorkerConfig},
};
use fanfold_domain::{
    breaker::BreakerConfig,
    failure::FailureKind,
    job::{JobId, JobSource, JobState, Resolution},
    retry::RetryPolicy,
    work::WorkId,
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
                WorkId::new(work).unwrap(),
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
    assert_eq!(h.library.published()[0].0, "My Story [5].epub");
    assert_eq!(h.downloader.requests()[0].existing, None);
    assert_eq!(
        h.notifier.events(),
        [NotifyEvent::Downloaded {
            work: WorkId::new(5).unwrap(),
            title: "My Story".into(),
            updated: false
        }]
    );
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
        published[1].0, "My Story [5].epub",
        "path must not move: readers key progress on it"
    );
    assert_eq!(
        h.downloader.requests()[1]
            .existing
            .as_deref()
            .and_then(|p| p.to_str()),
        Some("/library/My Story [5].epub"),
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
    h.library.forget("My Story [5].epub");
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
        "My Story [5].epub",
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
    assert_eq!(h.library.published()[1].0, "My Story [5].epub");
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
    assert_eq!(
        h.notifier.events(),
        [NotifyEvent::SiteDegraded {
            kind: FailureKind::SiteUnavailable
        }],
        "announced exactly once"
    );
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
    assert_eq!(
        h.notifier.events().last(),
        Some(&NotifyEvent::SiteRecovered)
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
    assert_eq!(h.library.published()[0].0, "The Long Fic [31337].epub");
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
