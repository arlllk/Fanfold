//! The real UI over realistic in-memory data, for design work and screenshots.
//!
//!   cargo run -p fanfold-adapter-web --example demo [-- --degraded] [--port 8099]
//!
//! Nothing here touches a database, the network or AO3.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::print_stdout,
    clippy::indexing_slicing,
    clippy::clone_on_ref_ptr
)]

use std::{
    sync::{Arc, Mutex},
    time::Duration,
};

use fanfold_adapter_web::{WebDeps, ops_router, ui_router};
use fanfold_app::{
    admin::JobAdmin,
    ports::{
        BookCatalog, BookRecord, Clock, JobRepository, LogEntry, LogLevel, SiteStatus,
        SiteStatusSource,
    },
    stats::WorkerStats,
};
use fanfold_domain::{
    Timestamp,
    failure::FailureKind,
    job::{JobSource, JobState},
    retry::RetryPolicy,
    timestamp,
    work::WorkId,
};
use fanfold_testkit::memory::InMemoryJobs;

struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> Timestamp {
        Timestamp::now()
    }
}

struct Site(Mutex<SiteStatus>);

impl SiteStatusSource for Site {
    fn site_status(&self) -> SiteStatus {
        *self.0.lock().unwrap()
    }
}

const POLICY: RetryPolicy = RetryPolicy {
    max_attempts: 5,
    base: Duration::from_secs(60),
    cap: Duration::from_secs(3600),
};

fn ago(minutes: u64) -> Timestamp {
    let now = Timestamp::now();
    now.checked_sub(jiff::SignedDuration::from_secs(
        i64::try_from(minutes * 60).unwrap(),
    ))
    .unwrap()
}

async fn title(repo: &InMemoryJobs, work: u64, title: &str, at: Timestamp) {
    repo.upsert(&BookRecord {
        work: WorkId::new(work).unwrap(),
        file_name: format!("{title} [{work}].epub"),
        title: title.to_owned(),
        updated_at: at,
    })
    .await
    .unwrap();
}

async fn log(
    repo: &InMemoryJobs,
    id: fanfold_domain::job::JobId,
    at: Timestamp,
    level: LogLevel,
    message: &str,
) {
    repo.append_log(
        id,
        &LogEntry {
            at,
            attempt: 0,
            level,
            message: message.to_owned(),
        },
    )
    .await
    .unwrap();
}

/// Queue, claim and settle one job with realistic timestamps.
async fn job(
    repo: &InMemoryJobs,
    work: u64,
    name: &str,
    started_minutes_ago: u64,
    outcome: Option<Result<(), (FailureKind, &str)>>,
) -> fanfold_domain::job::JobId {
    let t0 = ago(started_minutes_ago);
    let fanfold_app::ports::EnqueueOutcome::Created(created) = repo
        .enqueue(WorkId::new(work).unwrap().into(), JobSource::Mail, t0)
        .await
        .unwrap()
    else {
        panic!("duplicate work {work}");
    };
    if !name.is_empty() {
        title(repo, work, name, t0).await;
    }
    let Some(outcome) = outcome else {
        return created.id;
    };
    let t1 = timestamp::add(t0, Duration::from_secs(20));
    let mut claimed = loop {
        let j = repo.claim_next(t1).await.unwrap().unwrap();
        if j.id == created.id {
            break j;
        }
        let mut other = j;
        let at = other.updated_at;
        other.requeue_orphan(at).unwrap();
        repo.save(&other).await.unwrap();
    };
    log(repo, created.id, t1, LogLevel::Info, "starting attempt 1").await;
    let t2 = timestamp::add(t0, Duration::from_secs(48));
    match outcome {
        Ok(()) => {
            claimed.succeed(t2).unwrap();
            log(
                repo,
                created.id,
                t2,
                LogLevel::Info,
                &format!("downloaded \"{name}\""),
            )
            .await;
        }
        Err((kind, message)) => {
            claimed
                .fail(t2, kind, message.to_owned(), &POLICY, 0.5)
                .unwrap();
            log(repo, created.id, t2, LogLevel::Warn, "FFF: HTTP Error in FFF 'Cloudflare 525' (525) URL: https://archiveofourown.org/works/1\nretry_http_525_failures: waiting 8 s").await;
            log(
                repo,
                created.id,
                t2,
                LogLevel::Error,
                &format!("failed ({kind}): {message}; retrying"),
            )
            .await;
        }
    }
    repo.save(&claimed).await.unwrap();
    created.id
}

#[tokio::main]
async fn main() {
    let args: Vec<String> = std::env::args().collect();
    let degraded = args.iter().any(|a| a == "--degraded");
    let port: u16 = args
        .iter()
        .position(|a| a == "--port")
        .and_then(|i| args.get(i + 1))
        .and_then(|p| p.parse().ok())
        .unwrap_or(8099);

    let repo = Arc::new(InMemoryJobs::new());
    let stories = [
        "The Long Way Home",
        "A Study in Unreasonable Doubt",
        "Hold Fast, Sail On",
        "Everything the Tide Takes",
        "Small Hours",
        "Paper Lanterns",
        "What the River Remembers",
    ];
    // A shelf of history: mostly done, a couple of failures.
    for n in 0..26u64 {
        let name = stories[(n % 7) as usize];
        let fail = n == 7 || n == 19;
        let outcome = if fail {
            Err((FailureKind::NotFound, "deleted by the author"))
        } else {
            Ok(())
        };
        job(
            &repo,
            1000 + n,
            &format!("{name} {}", n / 7 + 1),
            600 - n * 20,
            Some(outcome),
        )
        .await;
    }
    // Needs attention.
    let flaky = job(
        &repo,
        2001,
        "Everything the Tide Takes",
        14,
        Some(Err((
            FailureKind::SiteUnavailable,
            "HTTP Error 525 from archiveofourown.org",
        ))),
    )
    .await;
    for (minutes, error) in [
        (9, "HTTP Error 525 from archiveofourown.org"),
        (4, "Cloudflare error code: 525 (through FlareSolverr)"),
    ] {
        let at = ago(minutes);
        log(&repo, flaky, at, LogLevel::Info, "starting attempt 1").await;
        let line = format!("failed (site_unavailable): {error}; retrying");
        log(&repo, flaky, at, LogLevel::Error, &line).await;
    }
    job(&repo, 2002, "", 40, Some(Err((FailureKind::AuthFailed, "Login failed: check the AO3 username and password. (This story is restricted.)\nsecond line of detail that is not shown in the list")))).await;
    // In progress.
    job(&repo, 3001, "A Study in Unreasonable Doubt, Volume II: The Reckoning of Small Hours and Other Long Titles", 1, None).await;
    job(&repo, 3002, "", 1, None).await;

    let clock: Arc<dyn Clock> = Arc::new(SystemClock);
    let status = if degraded {
        SiteStatus::Degraded {
            retry_at: timestamp::add(Timestamp::now(), Duration::from_mins(4)),
        }
    } else {
        SiteStatus::Healthy
    };
    let (stop, stopped) = tokio::sync::watch::channel(false);
    let deps = Arc::new(WebDeps {
        jobs: repo.clone(),
        catalog: repo.clone(),
        metadata: repo.clone(),
        admin: Arc::new(JobAdmin::new(repo.clone(), clock.clone())),
        session: Arc::new(fanfold_app::session::SessionAdmin::new(
            Arc::new(fanfold_testkit::memory::InMemorySession::new()),
            clock.clone(),
            "archiveofourown.org",
            std::time::Duration::from_hours(24),
        )),
        events: repo.clone(),
        site: Arc::new(Site(Mutex::new(status))),
        clock,
        stats: Arc::new(WorkerStats::new()),
        mail: Arc::new(fanfold_app::stats::MailStats::new()),
        readiness: repo,
        shutdown: stopped,
        max_attempts: 5,
        page_size: 8,
        version: "demo",
    });
    let _ = (ops_router, JobState::Queued);
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", port))
        .await
        .unwrap();
    println!("demo UI on http://127.0.0.1:{port}  (Ctrl-C to stop)");
    axum::serve(listener, ui_router(deps)).await.unwrap();
    drop(stop);
}
