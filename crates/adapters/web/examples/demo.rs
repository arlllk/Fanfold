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

/// What AO3 "said" about a demo work: invented, varied metadata for the library page.
#[allow(clippy::too_many_lines, reason = "one fixture, mostly data")]
async fn observe(repo: &InMemoryJobs, work: u64, title: &str, n: u64, at: Timestamp) {
    use fanfold_domain::metadata::{
        Author, Chapter, Date, SeriesPart, Tag, TagKind, WorkFacts, WorkMetadata, WorkStats,
    };
    const FANDOMS: [&str; 5] = [
        "Original Work",
        "The Glass Orchard - Mara Vell",
        "Starfall Academy (Video Game)",
        "Harbor Lights (Podcast)",
        "Lighthouse Chronicles (Web Series)",
    ];
    const AUTHORS: [&str; 5] = [
        "quillandink",
        "paperboats",
        "nightjar_writes",
        "Marigold & Thistle",
        "lanternfish",
    ];
    const RATINGS: [&str; 5] = [
        "General Audiences",
        "Teen And Up Audiences",
        "Mature",
        "Explicit",
        "Not Rated",
    ];
    const SHIPS: [&str; 4] = [
        "Ada Marsh/Ben Ward",
        "Ilse Harrow & Corin Vale",
        "Juniper/Theo Lark",
        "Wren Okafor/Sam Delacroix",
    ];
    const TROPES: [&str; 6] = [
        "Found Family",
        "Slow Burn",
        "Hurt/Comfort",
        "Fluff and Angst",
        "Enemies to Lovers",
        "Canon Divergence",
    ];
    let i = |k: u64, len: usize| usize::try_from(k).unwrap() % len;
    let posted = 1 + u32::try_from(n % 9).unwrap() * 3;
    let total = match n % 3 {
        0 => Some(posted),
        1 => Some(posted + 6),
        _ => None,
    };
    let author = AUTHORS[i(n, 5)];
    let day = |offset: u64| {
        Date::new(2026, 1, 1)
            .unwrap()
            .checked_add(jiff::Span::new().days(i64::try_from(offset).unwrap()))
            .unwrap()
    };
    let published = day(n * 9 % 300);
    let updated = day(n * 9 % 300 + 4 + n % 17);
    let fandom = FANDOMS[i(n, 5)];
    let mut tags = vec![
        Tag {
            kind: TagKind::Fandom,
            name: fandom.into(),
        },
        Tag {
            kind: TagKind::Warning,
            name: "No Archive Warnings Apply".into(),
        },
        Tag {
            kind: TagKind::Category,
            name: if n.is_multiple_of(2) { "F/M" } else { "Gen" }.into(),
        },
        Tag {
            kind: TagKind::Relationship,
            name: SHIPS[i(n, 4)].into(),
        },
        Tag {
            kind: TagKind::Character,
            name: "Ada Marsh".into(),
        },
        Tag {
            kind: TagKind::Character,
            name: "Ben Ward".into(),
        },
    ];
    for k in 0..3 {
        tags.push(Tag {
            kind: TagKind::Freeform,
            name: TROPES[i(n + k * 2, 6)].into(),
        });
    }
    let meta = WorkMetadata {
        work: WorkId::new(work).unwrap(),
        facts: WorkFacts {
            title: title.into(),
            byline: author.into(),
            summary_html: format!(
                "<p>The lighthouse has been dark for {} years, and someone has to climb the stairs.</p>\
                 <p>A story about keeping the light on &amp; the people who keep it with you.</p>",
                n + 3
            ),
            language: "English".into(),
            rating: RATINGS[i(n, 5)].into(),
            restricted: n % 8 == 3,
            status: if total == Some(posted) {
                "Completed"
            } else {
                "In-Progress"
            }
            .into(),
            published_on: Some(published),
            updated_on: Some(updated),
            chapters_posted: posted,
            chapters_total: total,
            words: 2_400 + n * 7_919 % 180_000,
        },
        authors: vec![Author {
            user: author.into(),
            pseud: author.into(),
        }],
        tags,
        series: if n % 4 == 1 {
            vec![SeriesPart {
                id: fanfold_domain::work::SeriesId::new(5001).unwrap(),
                name: "The Harbor Saga".into(),
                part: Some(u32::try_from(n / 4 + 1).unwrap()),
            }]
        } else {
            vec![]
        },
        chapters: (1..=posted)
            .map(|c| Chapter {
                number: c,
                title: format!(
                    "{c}. {}",
                    [
                        "First Light",
                        "The Logbook",
                        "Low Tide",
                        "Salt and Glass",
                        "Fog Signals"
                    ][usize::try_from(c).unwrap() % 5]
                ),
                ao3_id: Some(u64::from(c) + work * 100),
                posted_on: Some(day(n * 9 % 300 + u64::from(c))),
            })
            .collect(),
        stats: WorkStats {
            hits: Some(1_200 + n * 3_571 % 90_000),
            kudos: Some(40 + n * 613 % 6_000),
            comments: Some(5 + n * 37 % 900),
            bookmarks: Some(3 + n * 97 % 1_500),
        },
        raw: serde_json::json!({}),
    };
    fanfold_app::ports::MetadataStore::record_work(repo, &meta, at, None)
        .await
        .unwrap();
}

/// The delete buttons, over an in-memory library.
fn library_admin(
    repo: &Arc<InMemoryJobs>,
    clock: &Arc<dyn Clock>,
) -> Arc<fanfold_app::library::LibraryAdmin> {
    Arc::new(fanfold_app::library::LibraryAdmin::new(
        repo.clone(),
        repo.clone(),
        Arc::new(fanfold_testkit::memory::InMemoryLibrary::new()),
        repo.clone(),
        clock.clone(),
    ))
}

/// The library pages, over the demo's books and the real (in-memory) index.
fn browser(repo: &Arc<InMemoryJobs>) -> Arc<fanfold_app::browse::LibraryBrowser> {
    Arc::new(fanfold_app::browse::LibraryBrowser::new(
        repo.clone(),
        Arc::new(fanfold_adapter_search::TantivyIndexer),
    ))
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
        let full = format!("{name} {}", n / 7 + 1);
        job(&repo, 1000 + n, &full, 600 - n * 20, Some(outcome)).await;
        // The library: most books with what AO3 said; failures and one book left unchecked.
        if !fail && n != 5 {
            observe(&repo, 1000 + n, &full, n, ago(600 - n * 20)).await;
        }
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
        (4, "Cloudflare error code: 525"),
    ] {
        let at = ago(minutes);
        log(&repo, flaky, at, LogLevel::Info, "starting attempt 1").await;
        let line = format!("failed (site_unavailable): {error}; retrying");
        log(&repo, flaky, at, LogLevel::Error, &line).await;
    }
    job(&repo, 2002, "", 40, Some(Err((FailureKind::AuthFailed, "Login failed: check the AO3 username and password. (This story is restricted.)\nsecond line of detail that is not shown in the list")))).await;
    // Failed, then fixed by a later job: resolved, no attention needed.
    let resolve = "Login failed: check the AO3 username and password.";
    job(
        &repo,
        2003,
        "The Second Try",
        300,
        Some(Err((FailureKind::AuthFailed, resolve))),
    )
    .await;
    job(&repo, 2003, "The Second Try", 60, Some(Ok(()))).await;
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
        listing: repo.clone(),
        browser: browser(&repo),
        admin: Arc::new(JobAdmin::new(repo.clone(), clock.clone())),
        library: library_admin(&repo, &clock),
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
