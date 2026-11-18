//! Runs the shared port contracts against a real PostgreSQL.
//!
//! Needs a server and `TEST_DATABASE_URL` (admin access, e.g.
//! `postgres://postgres@localhost:5432/postgres`); each contract gets its own fresh
//! database. Use `just test-postgres` to run against a throwaway local server.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::{
    str::FromStr,
    sync::{
        Arc,
        atomic::{AtomicU32, Ordering},
    },
};

use fanfold_adapter_postgres::{PgStore, events::PgEvents};
use fanfold_app::ports::{
    BookCatalog, CursorStore, EventSource, JobRepository, MetadataStore, SessionStore,
};
use sqlx::{
    ConnectOptions, Connection,
    postgres::{PgConnectOptions, PgPoolOptions},
};

static NEXT_DB: AtomicU32 = AtomicU32::new(0);

async fn fresh_store() -> PgStore {
    let admin_url = std::env::var("TEST_DATABASE_URL").expect("TEST_DATABASE_URL must be set");
    let admin = PgConnectOptions::from_str(&admin_url).unwrap();
    let name = format!(
        "fanfold_t_{}_{}",
        std::process::id(),
        NEXT_DB.fetch_add(1, Ordering::Relaxed)
    );
    let mut conn = admin.connect().await.unwrap();
    // `name` is built from a pid and a counter, never from outside input.
    sqlx::query(sqlx::AssertSqlSafe(format!("CREATE DATABASE {name}")))
        .execute(&mut conn)
        .await
        .unwrap();
    conn.close().await.unwrap();
    let pool = PgPoolOptions::new()
        .max_connections(16)
        .connect_with(admin.database(&name))
        .await
        .unwrap();
    let store = PgStore::from_pool(pool);
    store.migrate().await.unwrap();
    store
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs PostgreSQL (TEST_DATABASE_URL)"]
async fn job_repository_contract() {
    fanfold_testkit::contracts::job_repository(|| async {
        Arc::new(fresh_store().await) as Arc<dyn JobRepository>
    })
    .await;
}

#[tokio::test]
#[ignore = "needs PostgreSQL (TEST_DATABASE_URL)"]
async fn book_catalog_contract() {
    fanfold_testkit::contracts::book_catalog(|| async {
        Arc::new(fresh_store().await) as Arc<dyn BookCatalog>
    })
    .await;
}

#[tokio::test]
#[ignore = "needs PostgreSQL (TEST_DATABASE_URL)"]
async fn migrations_are_idempotent_and_ping_works() {
    let store = fresh_store().await;
    store.migrate().await.unwrap();
    store.ping().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs PostgreSQL (TEST_DATABASE_URL)"]
async fn event_source_contract() {
    // Keep the shutdown sender alive for the whole test.
    let (_stop, stopped) = tokio::sync::watch::channel(false);
    fanfold_testkit::contracts::event_source(|| {
        let stopped = stopped.clone();
        async move {
            let store = fresh_store().await;
            let events = PgEvents::start(store.pool().clone(), stopped)
                .await
                .unwrap();
            (
                Arc::new(store) as Arc<dyn JobRepository>,
                Arc::new(events) as Arc<dyn EventSource>,
            )
        }
    })
    .await;
}

#[tokio::test]
#[ignore = "needs PostgreSQL (TEST_DATABASE_URL)"]
async fn cursor_store_contract() {
    fanfold_testkit::contracts::cursor_store(|| async {
        Arc::new(fresh_store().await) as Arc<dyn CursorStore>
    })
    .await;
}

#[tokio::test]
#[ignore = "needs PostgreSQL (TEST_DATABASE_URL)"]
async fn session_store_contract() {
    fanfold_testkit::contracts::session_store(|| async {
        Arc::new(fresh_store().await) as Arc<dyn SessionStore>
    })
    .await;
}

#[tokio::test]
#[ignore = "needs PostgreSQL (TEST_DATABASE_URL)"]
async fn metadata_store_contract() {
    fanfold_testkit::contracts::metadata_store(|| async {
        Arc::new(fresh_store().await) as Arc<dyn MetadataStore>
    })
    .await;
}

/// The analysis views answer "with which update was this tag added?" directly.
#[tokio::test]
#[ignore = "needs PostgreSQL (TEST_DATABASE_URL)"]
async fn the_tag_timeline_view_names_the_update_that_added_a_tag() {
    use fanfold_domain::{
        Timestamp,
        metadata::{Tag, TagKind},
    };
    let store = fresh_store().await;
    let t = |s| Timestamp::from_second(s).unwrap();
    let first = fanfold_testkit::contracts::observed_work(5);
    store.record_work(&first, t(1_000), None).await.unwrap();
    let mut second = first.clone();
    second.facts.updated_on = Some("2026-11-01".parse().unwrap());
    second.facts.chapters_posted = 3;
    second.tags.push(Tag {
        kind: TagKind::Freeform,
        name: "Yandere".into(),
    });
    store.record_work(&second, t(2_000), None).await.unwrap();

    let (update, chapters): (String, i32) = sqlx::query_as(
        "SELECT to_char(added_with_update_of, 'YYYY-MM-DD'), chapters_when_added
         FROM work_tag_timeline WHERE work_id = 5 AND name = 'Yandere'",
    )
    .fetch_one(store.pool())
    .await
    .unwrap();
    assert_eq!((update.as_str(), chapters), ("2026-11-01", 3));
    let versions: i64 = sqlx::query_scalar("SELECT count(*) FROM work_timeline WHERE work_id = 5")
        .fetch_one(store.pool())
        .await
        .unwrap();
    assert_eq!(versions, 2);
    let raw: serde_json::Value =
        sqlx::query_scalar("SELECT raw FROM work_versions WHERE work_id = 5 ORDER BY id LIMIT 1")
            .fetch_one(store.pool())
            .await
            .unwrap();
    assert_eq!(raw["numWords"], "179,890", "the raw answer is kept as is");
}

/// The deployed database already has jobs and books: 0006 must upgrade it, keeping the jobs
/// (as work jobs) and starting the library afresh.
#[tokio::test]
#[ignore = "needs PostgreSQL (TEST_DATABASE_URL)"]
async fn migration_0006_upgrades_an_existing_database() {
    let admin_url = std::env::var("TEST_DATABASE_URL").expect("TEST_DATABASE_URL must be set");
    let admin = PgConnectOptions::from_str(&admin_url).unwrap();
    let name = format!("fanfold_upgrade_{}", std::process::id());
    let mut conn = admin.connect().await.unwrap();
    sqlx::query(sqlx::AssertSqlSafe(format!("CREATE DATABASE {name}")))
        .execute(&mut conn)
        .await
        .unwrap();
    conn.close().await.unwrap();
    let pool = PgPoolOptions::new()
        .connect_with(admin.database(&name))
        .await
        .unwrap();

    let all = sqlx::migrate!("./migrations");
    let mut before = sqlx::migrate!("./migrations");
    before.migrations = all
        .migrations
        .iter()
        .filter(|m| m.version < 6)
        .cloned()
        .collect::<Vec<_>>()
        .into();
    before.run(&pool).await.unwrap();
    sqlx::query(
        "INSERT INTO jobs (work_id, source, state, attempt, next_attempt_at_ms, created_at_ms, updated_at_ms)
         VALUES (94081816, 'mail', 'succeeded', 0, 1, 1, 1), (95124081, 'mail', 'queued', 0, 1, 1, 1)",
    )
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query("INSERT INTO books (work_id, file_name, title, updated_at_ms) VALUES (94081816, 'Old [94081816].epub', 'Old', 1)")
        .execute(&pool)
        .await
        .unwrap();

    let store = PgStore::from_pool(pool);
    store.migrate().await.unwrap();
    let job = JobRepository::get(&store, fanfold_domain::job::JobId::new(2))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        job.target,
        fanfold_domain::job::Target::Work(fanfold_domain::work::WorkId::new(95_124_081).unwrap())
    );
    assert_eq!(
        BookCatalog::get(
            &store,
            fanfold_domain::work::WorkId::new(94_081_816).unwrap()
        )
        .await
        .unwrap(),
        None,
        "books start afresh under the new layout"
    );
}
