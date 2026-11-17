//! PostgreSQL implementation of the job queue and book catalog.
//!
//! * Queue claiming uses `FOR UPDATE SKIP LOCKED`, so any number of workers (or
//!   replicas) can pull concurrently without double delivery.
//! * "One active job per work" is a partial unique index, enforced by the database.
//! * Queries are plain runtime-checked SQL, so building needs no live database.
//!   The contract suite in `tests/` is what verifies them.

use std::{collections::HashMap, time::Duration};

pub mod events;

use async_trait::async_trait;
use fanfold_app::{
    error::RepoError,
    ports::{
        BookCatalog, BookRecord, CursorStore, EnqueueOutcome, JobCounts, JobFilter, JobRepository,
        LogEntry, LogLevel, Readiness, SessionStore,
    },
};
use fanfold_domain::{
    Timestamp,
    job::{Job, JobId, JobSource, JobState},
    session::BrowserSession,
    timestamp::{from_millis, to_millis},
    work::WorkId,
};
use sqlx::{
    PgPool, Row,
    postgres::{PgConnectOptions, PgPoolOptions, PgRow, PgSslMode},
};

/// Connection settings as separate parts, so the password never sits inside a URL
/// string that might end up in a log line.
pub struct PgSettings {
    pub host: String,
    pub port: u16,
    pub database: String,
    pub user: String,
    pub password: String,
    pub ssl_mode: PgSslMode,
    pub max_connections: u32,
}

impl std::fmt::Debug for PgSettings {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PgSettings")
            .field("host", &self.host)
            .field("port", &self.port)
            .field("database", &self.database)
            .field("user", &self.user)
            .finish_non_exhaustive()
    }
}

#[derive(Debug, Clone)]
pub struct PgStore {
    pool: PgPool,
}

fn backend(e: &sqlx::Error) -> RepoError {
    RepoError::Backend(e.to_string())
}

macro_rules! job_columns {
    () => {
        "id, work_id, source, state, attempt, next_attempt_at_ms, created_at_ms, updated_at_ms, last_error, force"
    };
}

fn ts(ms: i64) -> Result<Timestamp, RepoError> {
    from_millis(ms).ok_or_else(|| RepoError::Backend(format!("timestamp out of range: {ms}")))
}

fn job_from_row(row: &PgRow) -> Result<Job, RepoError> {
    let corrupt = |what: &str| RepoError::Backend(format!("corrupt job row: {what}"));
    let source: String = row.try_get("source").map_err(|e| backend(&e))?;
    let state: String = row.try_get("state").map_err(|e| backend(&e))?;
    let attempt: i32 = row.try_get("attempt").map_err(|e| backend(&e))?;
    Ok(Job {
        id: JobId::new(row.try_get("id").map_err(|e| backend(&e))?),
        work: WorkId::from_i64(row.try_get("work_id").map_err(|e| backend(&e))?)
            .ok_or_else(|| corrupt("work_id"))?,
        source: source.parse::<JobSource>().map_err(|_| corrupt("source"))?,
        state: state.parse::<JobState>().map_err(|_| corrupt("state"))?,
        attempt: u32::try_from(attempt).map_err(|_| corrupt("attempt"))?,
        next_attempt_at: ts(row.try_get("next_attempt_at_ms").map_err(|e| backend(&e))?)?,
        created_at: ts(row.try_get("created_at_ms").map_err(|e| backend(&e))?)?,
        updated_at: ts(row.try_get("updated_at_ms").map_err(|e| backend(&e))?)?,
        last_error: row.try_get("last_error").map_err(|e| backend(&e))?,
        force: row.try_get("force").map_err(|e| backend(&e))?,
    })
}

impl PgStore {
    pub async fn connect(settings: &PgSettings) -> Result<Self, RepoError> {
        let options = PgConnectOptions::new()
            .host(&settings.host)
            .port(settings.port)
            .database(&settings.database)
            .username(&settings.user)
            .password(&settings.password)
            .ssl_mode(settings.ssl_mode)
            .application_name("fanfold");
        let pool = PgPoolOptions::new()
            .max_connections(settings.max_connections)
            .acquire_timeout(Duration::from_secs(10))
            .connect_with(options)
            .await
            .map_err(|e| backend(&e))?;
        Ok(Self { pool })
    }

    #[must_use]
    pub const fn from_pool(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Apply pending migrations (idempotent; safe at every start).
    pub async fn migrate(&self) -> Result<(), RepoError> {
        sqlx::migrate!("./migrations")
            .run(&self.pool)
            .await
            .map_err(|e| RepoError::Backend(format!("migration failed: {e}")))
    }

    /// The connection pool, for components that need their own connection (the
    /// event listener).
    #[must_use]
    pub const fn pool(&self) -> &PgPool {
        &self.pool
    }

    /// Cheap liveness probe for `/readyz`.
    pub async fn ping(&self) -> Result<(), RepoError> {
        sqlx::query("SELECT 1")
            .execute(&self.pool)
            .await
            .map(|_| ())
            .map_err(|e| backend(&e))
    }
}

#[async_trait]
impl JobRepository for PgStore {
    async fn enqueue(
        &self,
        work: WorkId,
        source: JobSource,
        now: Timestamp,
    ) -> Result<EnqueueOutcome, RepoError> {
        let now_ms = to_millis(now);
        // Two statements can race with a job finishing in between; a few retries
        // make that window irrelevant.
        for _ in 0..3 {
            let inserted = sqlx::query(concat!("INSERT INTO jobs (work_id, source, state, attempt, next_attempt_at_ms, created_at_ms, updated_at_ms)
                 VALUES ($1, $2, 'queued', 0, $3, $3, $3)
                 ON CONFLICT DO NOTHING
                 RETURNING ", job_columns!(), ""))
            .bind(work.to_i64())
            .bind(source.as_str())
            .bind(now_ms)
            .fetch_optional(&self.pool)
            .await
            .map_err(|e| backend(&e))?;
            if let Some(row) = inserted {
                return Ok(EnqueueOutcome::Created(job_from_row(&row)?));
            }
            let active: Option<i64> = sqlx::query_scalar(
                "SELECT id FROM jobs WHERE work_id = $1 AND state IN ('queued', 'running')",
            )
            .bind(work.to_i64())
            .fetch_optional(&self.pool)
            .await
            .map_err(|e| backend(&e))?;
            if let Some(id) = active {
                return Ok(EnqueueOutcome::AlreadyActive(JobId::new(id)));
            }
        }
        Err(RepoError::Backend("could not enqueue: contention".into()))
    }

    async fn claim_next(&self, now: Timestamp) -> Result<Option<Job>, RepoError> {
        let now_ms = to_millis(now);
        let row = sqlx::query(concat!(
            "UPDATE jobs SET state = 'running', updated_at_ms = $1
             WHERE id = (
                 SELECT id FROM jobs
                 WHERE state = 'queued' AND next_attempt_at_ms <= $1
                 ORDER BY next_attempt_at_ms, id
                 FOR UPDATE SKIP LOCKED
                 LIMIT 1
             )
             RETURNING ",
            job_columns!(),
            ""
        ))
        .bind(now_ms)
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| backend(&e))?;
        row.as_ref().map(job_from_row).transpose()
    }

    async fn save(&self, job: &Job) -> Result<(), RepoError> {
        let result = sqlx::query(
            "UPDATE jobs SET state = $2, attempt = $3, next_attempt_at_ms = $4, updated_at_ms = $5, last_error = $6,
                 force = $7
             WHERE id = $1",
        )
        .bind(job.id.get())
        .bind(job.state.as_str())
        .bind(i32::try_from(job.attempt).unwrap_or(i32::MAX))
        .bind(to_millis(job.next_attempt_at))
        .bind(to_millis(job.updated_at))
        .bind(job.last_error.as_deref())
        .bind(job.force)
        .execute(&self.pool)
        .await
        .map_err(|e| match &e {
            // unique_violation on jobs_one_active_per_work
            sqlx::Error::Database(db) if db.code().as_deref() == Some("23505") => {
                RepoError::Conflict("the work already has an active job".into())
            }
            _ => backend(&e),
        })?;
        if result.rows_affected() == 0 {
            return Err(RepoError::NotFound(job.id));
        }
        Ok(())
    }

    async fn get(&self, id: JobId) -> Result<Option<Job>, RepoError> {
        let row = sqlx::query(concat!(
            "SELECT ",
            job_columns!(),
            " FROM jobs WHERE id = $1"
        ))
        .bind(id.get())
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| backend(&e))?;
        row.as_ref().map(job_from_row).transpose()
    }

    async fn list(
        &self,
        filter: JobFilter,
        limit: u32,
        offset: u32,
    ) -> Result<Vec<Job>, RepoError> {
        let rows = sqlx::query(concat!(
            "SELECT ",
            job_columns!(),
            " FROM jobs WHERE CASE $1
                 WHEN 'all' THEN true
                 WHEN 'active' THEN state IN ('queued', 'running')
                 WHEN 'retrying' THEN state = 'queued' AND last_error IS NOT NULL
                 WHEN 'failed' THEN state = 'failed'
                 WHEN 'succeeded' THEN state = 'succeeded'
                 WHEN 'finished' THEN state IN ('succeeded', 'failed', 'cancelled')
                 ELSE false END
             ORDER BY updated_at_ms DESC, id DESC LIMIT $2 OFFSET $3"
        ))
        .bind(filter.as_str())
        .bind(i64::from(limit))
        .bind(i64::from(offset))
        .fetch_all(&self.pool)
        .await
        .map_err(|e| backend(&e))?;
        rows.iter().map(job_from_row).collect()
    }

    async fn counts(&self) -> Result<JobCounts, RepoError> {
        let row = sqlx::query(
            "SELECT
                 count(*) FILTER (WHERE state = 'queued' AND last_error IS NULL) AS queued,
                 count(*) FILTER (WHERE state = 'running') AS running,
                 count(*) FILTER (WHERE state = 'queued' AND last_error IS NOT NULL) AS retrying,
                 count(*) FILTER (WHERE state = 'succeeded') AS succeeded,
                 count(*) FILTER (WHERE state = 'failed') AS failed,
                 count(*) FILTER (WHERE state = 'cancelled') AS cancelled
             FROM jobs",
        )
        .fetch_one(&self.pool)
        .await
        .map_err(|e| backend(&e))?;
        let count = |name: &str| -> Result<u64, RepoError> {
            let n: i64 = row.try_get(name).map_err(|e| backend(&e))?;
            u64::try_from(n).map_err(|_| RepoError::Backend(format!("negative count for {name}")))
        };
        Ok(JobCounts {
            queued: count("queued")?,
            running: count("running")?,
            retrying: count("retrying")?,
            succeeded: count("succeeded")?,
            failed: count("failed")?,
            cancelled: count("cancelled")?,
        })
    }

    async fn recover_orphans(&self, now: Timestamp) -> Result<u64, RepoError> {
        let now_ms = to_millis(now);
        sqlx::query("UPDATE jobs SET state = 'queued', next_attempt_at_ms = $1, updated_at_ms = $1 WHERE state = 'running'")
            .bind(now_ms)
            .execute(&self.pool)
            .await
            .map(|r| r.rows_affected())
            .map_err(|e| backend(&e))
    }

    async fn append_log(&self, job: JobId, entry: &LogEntry) -> Result<(), RepoError> {
        sqlx::query("INSERT INTO job_logs (job_id, at_ms, attempt, level, message) VALUES ($1, $2, $3, $4, $5)")
            .bind(job.get())
            .bind(to_millis(entry.at))
            .bind(i32::try_from(entry.attempt).unwrap_or(i32::MAX))
            .bind(entry.level.as_str())
            .bind(&entry.message)
            .execute(&self.pool)
            .await
            .map(|_| ())
            .map_err(|e| match &e {
                // foreign_key_violation: no such job
                sqlx::Error::Database(db) if db.code().as_deref() == Some("23503") => RepoError::NotFound(job),
                _ => backend(&e),
            })
    }

    async fn logs(&self, job: JobId) -> Result<Vec<LogEntry>, RepoError> {
        let rows = sqlx::query(
            "SELECT at_ms, attempt, level, message FROM job_logs WHERE job_id = $1 ORDER BY id",
        )
        .bind(job.get())
        .fetch_all(&self.pool)
        .await
        .map_err(|e| backend(&e))?;
        rows.iter()
            .map(|row| {
                let level: String = row.try_get("level").map_err(|e| backend(&e))?;
                let attempt: i32 = row.try_get("attempt").map_err(|e| backend(&e))?;
                Ok(LogEntry {
                    at: ts(row.try_get("at_ms").map_err(|e| backend(&e))?)?,
                    attempt: u32::try_from(attempt).unwrap_or(0),
                    level: match level.as_str() {
                        "warn" => LogLevel::Warn,
                        "error" => LogLevel::Error,
                        _ => LogLevel::Info,
                    },
                    message: row.try_get("message").map_err(|e| backend(&e))?,
                })
            })
            .collect()
    }
}

#[async_trait]
impl BookCatalog for PgStore {
    async fn get(&self, work: WorkId) -> Result<Option<BookRecord>, RepoError> {
        let row =
            sqlx::query("SELECT file_name, title, updated_at_ms FROM books WHERE work_id = $1")
                .bind(work.to_i64())
                .fetch_optional(&self.pool)
                .await
                .map_err(|e| backend(&e))?;
        row.map(|row| {
            Ok(BookRecord {
                work,
                file_name: row.try_get("file_name").map_err(|e| backend(&e))?,
                title: row.try_get("title").map_err(|e| backend(&e))?,
                updated_at: ts(row.try_get("updated_at_ms").map_err(|e| backend(&e))?)?,
            })
        })
        .transpose()
    }

    async fn titles(&self, works: &[WorkId]) -> Result<HashMap<WorkId, String>, RepoError> {
        if works.is_empty() {
            return Ok(HashMap::new());
        }
        let ids: Vec<i64> = works.iter().map(|w| w.to_i64()).collect();
        let rows = sqlx::query("SELECT work_id, title FROM books WHERE work_id = ANY($1)")
            .bind(ids)
            .fetch_all(&self.pool)
            .await
            .map_err(|e| backend(&e))?;
        rows.iter()
            .map(|row| {
                let id: i64 = row.try_get("work_id").map_err(|e| backend(&e))?;
                let work = WorkId::from_i64(id)
                    .ok_or_else(|| RepoError::Backend("corrupt book row: work_id".into()))?;
                Ok((work, row.try_get("title").map_err(|e| backend(&e))?))
            })
            .collect()
    }

    /// `file_name` is deliberately absent from the `DO UPDATE` list: once a book
    /// has a path it keeps it.
    async fn upsert(&self, record: &BookRecord) -> Result<(), RepoError> {
        sqlx::query(
            "INSERT INTO books (work_id, file_name, title, updated_at_ms) VALUES ($1, $2, $3, $4)
             ON CONFLICT (work_id) DO UPDATE SET title = EXCLUDED.title, updated_at_ms = EXCLUDED.updated_at_ms",
        )
        .bind(record.work.to_i64())
        .bind(&record.file_name)
        .bind(&record.title)
        .bind(to_millis(record.updated_at))
        .execute(&self.pool)
        .await
        .map(|_| ())
        .map_err(|e| backend(&e))
    }
}

#[async_trait]
impl Readiness for PgStore {
    async fn check(&self) -> Result<(), RepoError> {
        self.ping().await
    }
}

#[async_trait]
impl CursorStore for PgStore {
    async fn get(&self, name: &str) -> Result<Option<String>, RepoError> {
        sqlx::query_scalar("SELECT value FROM cursors WHERE name = $1")
            .bind(name)
            .fetch_optional(&self.pool)
            .await
            .map_err(|e| backend(&e))
    }

    async fn set(&self, name: &str, value: &str) -> Result<(), RepoError> {
        sqlx::query(
            "INSERT INTO cursors (name, value) VALUES ($1, $2)
             ON CONFLICT (name) DO UPDATE SET value = EXCLUDED.value",
        )
        .bind(name)
        .bind(value)
        .execute(&self.pool)
        .await
        .map(|_| ())
        .map_err(|e| backend(&e))
    }
}

fn session_from_row(row: &sqlx::postgres::PgRow) -> Result<BrowserSession, RepoError> {
    let cookies: sqlx::types::Json<Vec<fanfold_domain::session::Cookie>> =
        row.try_get("cookies").map_err(|e| backend(&e))?;
    Ok(BrowserSession {
        user_agent: row.try_get("user_agent").map_err(|e| backend(&e))?,
        cookies: cookies.0,
        updated_at: ts(row.try_get("updated_at_ms").map_err(|e| backend(&e))?)?,
    })
}

#[async_trait]
impl SessionStore for PgStore {
    async fn load(&self) -> Result<Option<BrowserSession>, RepoError> {
        sqlx::query("SELECT user_agent, cookies, updated_at_ms FROM browser_session WHERE id = 1")
            .fetch_optional(&self.pool)
            .await
            .map_err(|e| backend(&e))?
            .as_ref()
            .map(session_from_row)
            .transpose()
    }

    async fn save(&self, session: &BrowserSession) -> Result<(), RepoError> {
        sqlx::query(
            "INSERT INTO browser_session (id, user_agent, cookies, updated_at_ms)
             VALUES (1, $1, $2, $3)
             ON CONFLICT (id) DO UPDATE SET user_agent = EXCLUDED.user_agent,
                 cookies = EXCLUDED.cookies, updated_at_ms = EXCLUDED.updated_at_ms",
        )
        .bind(&session.user_agent)
        .bind(sqlx::types::Json(&session.cookies))
        .bind(to_millis(session.updated_at))
        .execute(&self.pool)
        .await
        .map(|_| ())
        .map_err(|e| backend(&e))
    }

    async fn refresh(
        &self,
        session: &BrowserSession,
        based_on: Timestamp,
    ) -> Result<bool, RepoError> {
        sqlx::query(
            "UPDATE browser_session SET user_agent = $1, cookies = $2, updated_at_ms = $3
             WHERE id = 1 AND updated_at_ms = $4",
        )
        .bind(&session.user_agent)
        .bind(sqlx::types::Json(&session.cookies))
        .bind(to_millis(session.updated_at))
        .bind(to_millis(based_on))
        .execute(&self.pool)
        .await
        .map(|done| done.rows_affected() == 1)
        .map_err(|e| backend(&e))
    }

    async fn clear(&self) -> Result<bool, RepoError> {
        sqlx::query("DELETE FROM browser_session WHERE id = 1")
            .execute(&self.pool)
            .await
            .map(|done| done.rows_affected() == 1)
            .map_err(|e| backend(&e))
    }
}
