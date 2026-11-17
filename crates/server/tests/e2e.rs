//! End-to-end: config text -> secrets -> real PostgreSQL, filesystem and FanFicFare
//! (against its offline `test1.com` site) -> worker.
//!
//! Opt-in, because it needs external pieces:
//!   `just e2e`   (starts a throwaway PostgreSQL and uses `$FFD_PYTHON`)
//! or set `TEST_DATABASE_URL` and `FFD_PYTHON` yourself and run
//! `cargo test -p fanfold-server --test e2e -- --ignored`.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::clone_on_ref_ptr
)]

use std::{collections::HashMap, io, path::Path, str::FromStr, sync::Arc};

use fanfold_adapter_fanficfare::runner::TokioProcessRunner;
use fanfold_adapter_postgres::PgStore;
use fanfold_app::{
    ports::{BookCatalog, Clock, EnqueueOutcome, JobRepository},
    worker::{Outcome, Tick},
};
use fanfold_domain::{
    job::{JobSource, JobState},
    work::WorkId,
};
use fanfold_server::{
    config::Config,
    infra::SystemClock,
    runtime::{build_worker_with, start_web},
    secrets::SecretSource,
};
use sqlx::{
    ConnectOptions, Connection,
    postgres::{PgConnectOptions, PgPoolOptions},
};

struct Env(HashMap<&'static str, &'static str>);

impl SecretSource for Env {
    fn env(&self, name: &str) -> Option<String> {
        self.0.get(name).map(|v| (*v).to_owned())
    }

    fn read_file(&self, path: &Path) -> io::Result<String> {
        Err(io::Error::new(
            io::ErrorKind::NotFound,
            path.display().to_string(),
        ))
    }
}

static NEXT: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

async fn fresh_store() -> Arc<PgStore> {
    let admin_url = std::env::var("TEST_DATABASE_URL").expect("TEST_DATABASE_URL must be set");
    let admin = PgConnectOptions::from_str(&admin_url).unwrap();
    let name = format!(
        "fanfold_e2e_{}_{}",
        std::process::id(),
        NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    );
    let mut conn = admin.connect().await.unwrap();
    // `name` is built from a pid, never from outside input.
    sqlx::query(sqlx::AssertSqlSafe(format!("CREATE DATABASE {name}")))
        .execute(&mut conn)
        .await
        .unwrap();
    conn.close().await.unwrap();
    let pool = PgPoolOptions::new()
        .max_connections(4)
        .connect_with(admin.database(&name))
        .await
        .unwrap();
    let store = PgStore::from_pool(pool);
    store.migrate().await.unwrap();
    Arc::new(store)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
// One long scenario on purpose: each step builds on the previous one.
#[allow(clippy::too_many_lines)]
#[ignore = "needs PostgreSQL (TEST_DATABASE_URL) and FFD_PYTHON"]
async fn a_queued_work_becomes_an_epub_in_the_library_and_updates_stay_quiet() {
    let python =
        std::env::var("FFD_PYTHON").expect("FFD_PYTHON must point at a Python with fanficfare");
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();

    let config: Config = format!(
        r#"
[database]
host = "unused-in-this-test"
name = "unused"
user = "unused"
password = {{ env = "DB_PASSWORD" }}

[library]
path = "{lib}"
scratch = "{scratch}"

[ao3]
username = {{ env = "AO3_USERNAME" }}
password = {{ env = "AO3_PASSWORD" }}

[downloader]
python = "{python}"
timeout = "2m"
personal_ini = "{ini}"
"#,
        lib = root.join("library").display(),
        scratch = root.join("scratch").display(),
        ini = root.join("run/personal.ini").display(),
    )
    .parse()
    .unwrap();

    // A password that exercises ini escaping through FanFicFare's real config parser.
    let secrets = config
        .resolve_secrets(&Env(HashMap::from([
            ("DB_PASSWORD", "db"),
            ("AO3_USERNAME", "tester"),
            ("AO3_PASSWORD", "100%sure: #1;"),
        ])))
        .unwrap();

    let store = fresh_store().await;
    let worker = build_worker_with(
        &config,
        &secrets,
        &store,
        Arc::new(TokioProcessRunner),
        "http://test1.com?sid={id}",
    )
    .unwrap();

    // The rendered credentials file is private and correctly escaped.
    let ini_path = root.join("run/personal.ini");
    let ini = std::fs::read_to_string(&ini_path).unwrap();
    assert!(ini.contains("password:100%%sure: #1;\n"), "{ini}");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(&ini_path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }

    // 1. First download.
    let work = WorkId::new(42).unwrap();
    let EnqueueOutcome::Created(job) = store
        .enqueue(work, JobSource::Manual, SystemClock.now())
        .await
        .unwrap()
    else {
        panic!("must create");
    };
    let tick = worker.run_once().await.unwrap();
    assert_eq!(
        tick,
        Tick::Processed {
            job: job.id,
            outcome: Outcome::Succeeded
        }
    );

    let book = root.join("library/Test Story Title 42 [42].epub");
    let size = std::fs::metadata(&book)
        .expect("the epub is in the library")
        .len();
    assert!(size > 1000, "{size}");
    let record = store.get_book(work).await;
    assert_eq!(record.file_name, "Test Story Title 42 [42].epub");
    assert_eq!(
        JobRepository::get(store.as_ref(), job.id)
            .await
            .unwrap()
            .unwrap()
            .state,
        JobState::Succeeded
    );
    let logs = store.logs(job.id).await.unwrap();
    assert!(
        logs.iter()
            .any(|l| l.message.contains("downloaded \"Test Story Title 42\"")),
        "{logs:#?}"
    );

    // Nothing is left behind in the scratch or staging directories.
    let listing = |dir: &str| -> Vec<String> {
        std::fs::read_dir(root.join(dir))
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect()
    };
    assert_eq!(
        listing("scratch"),
        Vec::<String>::new(),
        "scratch space leaked"
    );
    assert_eq!(
        listing("library/.tmp"),
        Vec::<String>::new(),
        "staging files leaked"
    );

    // 2. The author "updates": a new job, but the site has nothing new.
    let modified = std::fs::metadata(&book).unwrap().modified().unwrap();
    let EnqueueOutcome::Created(update) = store
        .enqueue(work, JobSource::Mail, SystemClock.now())
        .await
        .unwrap()
    else {
        panic!("a finished job must not block a new one");
    };
    let tick = worker.run_once().await.unwrap();
    assert_eq!(
        tick,
        Tick::Processed {
            job: update.id,
            outcome: Outcome::Succeeded
        }
    );
    assert_eq!(
        std::fs::metadata(&book).unwrap().modified().unwrap(),
        modified,
        "an unchanged update must not rewrite the file"
    );
    let logs = store.logs(update.id).await.unwrap();
    assert!(
        logs.iter()
            .any(|l| l.message.contains("already up to date")),
        "{logs:#?}"
    );

    // 3. Nothing else is queued.
    assert_eq!(worker.run_once().await.unwrap(), Tick::Idle);
}

/// Small helper trait so the test reads naturally.
trait BookLookup {
    async fn get_book(&self, work: WorkId) -> fanfold_app::ports::BookRecord;
}

impl BookLookup for Arc<PgStore> {
    async fn get_book(&self, work: WorkId) -> fanfold_app::ports::BookRecord {
        BookCatalog::get(self.as_ref(), work)
            .await
            .unwrap()
            .expect("book recorded")
    }
}

// --------------------------------------------------------------------- the web UI

/// Poll until `check` holds, or fail after five seconds.
async fn eventually<F: Fn() -> bool>(what: &str, check: F) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while !check() {
        assert!(
            std::time::Instant::now() < deadline,
            "timed out waiting for {what}"
        );
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
}

/// A browser's view of the whole stack: add a link, watch the live stream, read the page.
// One long scenario on purpose: each step builds on the previous one.
#[allow(clippy::too_many_lines)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs PostgreSQL (TEST_DATABASE_URL) and FFD_PYTHON"]
async fn the_ui_shows_a_download_live_through_postgres_notifications() {
    use std::sync::Mutex;

    let python =
        std::env::var("FFD_PYTHON").expect("FFD_PYTHON must point at a Python with fanficfare");
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let config: Config = format!(
        r#"
[database]
host = "unused"
name = "unused"
user = "unused"
password = {{ env = "DB_PASSWORD" }}

[library]
path = "{lib}"
scratch = "{scratch}"

[ao3]
username = {{ env = "AO3_USERNAME" }}
password = {{ env = "AO3_PASSWORD" }}

[downloader]
python = "{python}"
timeout = "2m"
personal_ini = "{ini}"

[web]
listen = "127.0.0.1:0"
ops_listen = "127.0.0.1:0"
"#,
        lib = root.join("library").display(),
        scratch = root.join("scratch").display(),
        ini = root.join("run/personal.ini").display(),
    )
    .parse()
    .unwrap();
    let secrets = config
        .resolve_secrets(&Env(HashMap::from([
            ("DB_PASSWORD", "db"),
            ("AO3_USERNAME", "tester"),
            ("AO3_PASSWORD", "pw"),
        ])))
        .unwrap();

    let store = fresh_store().await;
    let worker = Arc::new(
        build_worker_with(
            &config,
            &secrets,
            &store,
            Arc::new(TokioProcessRunner),
            "http://test1.com?sid={id}",
        )
        .unwrap(),
    );
    let (stop, stopped) = tokio::sync::watch::channel(false);
    let events =
        fanfold_adapter_postgres::events::PgEvents::start(store.pool().clone(), stopped.clone())
            .await
            .unwrap();
    let web = start_web(
        &config,
        &store,
        &worker,
        Arc::new(events),
        Arc::new(fanfold_app::stats::MailStats::new()),
        stopped,
    )
    .await
    .unwrap();
    let ui = format!("http://{}", web.ui_addr);
    let ops = format!("http://{}", web.ops_addr);
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .no_proxy()
        .build()
        .unwrap();

    // The browser opens the live stream (as the page's script does).
    let seen = Arc::new(Mutex::new(String::new()));
    let mut stream = client.get(format!("{ui}/events")).send().await.unwrap();
    assert_eq!(stream.headers()["content-type"], "text/event-stream");
    let sink = Arc::clone(&seen);
    tokio::spawn(async move {
        while let Ok(Some(chunk)) = stream.chunk().await {
            sink.lock()
                .unwrap()
                .push_str(&String::from_utf8_lossy(&chunk));
        }
    });

    // The user pastes a link.
    let reply = client
        .post(format!("{ui}/jobs"))
        .header("content-type", "application/x-www-form-urlencoded")
        .header("sec-fetch-site", "same-origin")
        .body("url=https%3A%2F%2Farchiveofourown.org%2Fworks%2F42")
        .send()
        .await
        .unwrap();
    assert_eq!(reply.status(), 303);
    assert_eq!(reply.headers()["location"], "/?added=1");

    // PostgreSQL's trigger -> NOTIFY -> listener -> SSE reaches the browser.
    eventually("the enqueue event", || {
        seen.lock().unwrap().contains("data: job 1")
    })
    .await;

    // The page shows it as queued, rendered from the database.
    let page = client
        .get(format!("{ui}/jobs/1"))
        .header("x-fragment", "1")
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(
        page.contains("Queued") && page.contains("Work #42"),
        "{page}"
    );

    // The worker downloads it; the browser is told, and the next fetch shows the result.
    let before = seen.lock().unwrap().matches("data: job 1").count();
    let tick = worker.run_once().await.unwrap();
    assert!(matches!(
        tick,
        fanfold_app::worker::Tick::Processed {
            outcome: Outcome::Succeeded,
            ..
        }
    ));
    eventually("the completion events", || {
        seen.lock().unwrap().matches("data: job 1").count() > before
    })
    .await;
    eventually("log events", || {
        seen.lock().unwrap().contains("data: log 1")
    })
    .await;

    let page = client
        .get(format!("{ui}/jobs/1"))
        .header("x-fragment", "1")
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    for text in [
        "Test Story Title 42",
        "Done",
        "Run 1",
        "starting attempt 1",
        "Test Story Title 42 [42].epub",
    ] {
        assert!(page.contains(text), "missing {text:?} in {page}");
    }
    let dashboard = client
        .get(format!("{ui}/"))
        .header("x-fragment", "1")
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(
        dashboard.contains("Test Story Title 42") && dashboard.contains("Nothing needs attention."),
        "{dashboard}"
    );

    // Prometheus sees it too, on the separate ops port.
    let metrics = client
        .get(format!("{ops}/metrics"))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    for line in [
        "fanfold_downloads_total{outcome=\"downloaded\"} 1",
        "fanfold_jobs{state=\"succeeded\"} 1",
        "fanfold_site_degraded 0",
    ] {
        assert!(
            metrics.lines().any(|l| l == line),
            "missing {line:?} in:\n{metrics}"
        );
    }
    assert_eq!(
        client
            .get(format!("{ops}/readyz"))
            .send()
            .await
            .unwrap()
            .status(),
        200
    );
    assert_eq!(
        client
            .get(format!("{ui}/metrics"))
            .send()
            .await
            .unwrap()
            .status(),
        404,
        "metrics stay off the UI port"
    );

    // A browser tab left open must not block shutdown.
    stop.send(true).unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(5), web.join())
        .await
        .expect("servers stop even with an event stream still open");
}
