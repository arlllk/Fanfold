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
use fanfold_app::ports::{BookCatalog, CursorStore, EventSource, JobRepository, SessionStore};
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
