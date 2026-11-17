//! Use case: take one due job, download it, publish it, record the outcome.

use std::{
    fmt,
    sync::{Arc, Mutex, MutexGuard, PoisonError},
};

use fanfold_domain::{
    breaker::{BreakerConfig, BreakerEvent, CircuitBreaker},
    failure::FailureKind,
    job::{Job, JobId, Resolution},
    retry::RetryPolicy,
    work::book_file_name,
};

use crate::{
    error::AppError,
    ports::{
        BookCatalog, BookRecord, Clock, DownloadError, DownloadRequest, Downloader, Jitter,
        JobRepository, LibraryStore, LogEntry, LogLevel, Notifier, NotifyEvent, SiteStatus,
        SiteStatusSource,
    },
    stats::WorkerStats,
};

/// Everything the worker talks to.
#[derive(Clone)]
pub struct Ports {
    pub jobs: Arc<dyn JobRepository>,
    pub catalog: Arc<dyn BookCatalog>,
    pub downloader: Arc<dyn Downloader>,
    pub library: Arc<dyn LibraryStore>,
    pub notifier: Arc<dyn Notifier>,
    pub clock: Arc<dyn Clock>,
    pub jitter: Arc<dyn Jitter>,
}

impl fmt::Debug for Ports {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Ports").finish_non_exhaustive()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WorkerConfig {
    pub retry: RetryPolicy,
    pub breaker: BreakerConfig,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    Succeeded,
    Failed(Resolution),
}

/// Result of one [`Worker::run_once`] call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tick {
    /// Nothing due.
    Idle,
    /// The circuit breaker is open; the queue is deliberately not being drained.
    BreakerOpen,
    Processed {
        job: JobId,
        outcome: Outcome,
    },
}

pub struct Worker {
    ports: Ports,
    retry: RetryPolicy,
    breaker: Mutex<CircuitBreaker>,
    stats: Arc<WorkerStats>,
}

impl fmt::Debug for Worker {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Worker")
            .field("retry", &self.retry)
            .finish_non_exhaustive()
    }
}

/// A finished download that has been published to the library.
struct Published {
    title: String,
    /// The library file was replaced by a newer version of an existing book.
    updated: bool,
    /// Whether the library changed at all.
    changed: bool,
    log: String,
}

/// Every download attempt begins with a job-log line starting with this text; the
/// UI uses it to split a job's log into runs.
pub const ATTEMPT_START_PREFIX: &str = "starting attempt";

/// Logged at the start of a forced run.
pub const FORCED_NOTE: &str =
    "forced: downloading from scratch; the stored book is replaced only if this succeeds";

const MAX_LOG_CHARS: usize = 20_000;

impl Worker {
    #[must_use]
    pub fn new(ports: Ports, config: WorkerConfig) -> Self {
        Self {
            ports,
            retry: config.retry,
            breaker: Mutex::new(CircuitBreaker::new(config.breaker)),
            stats: Arc::new(WorkerStats::new()),
        }
    }

    /// Counters for the metrics endpoint.
    #[must_use]
    pub fn stats(&self) -> Arc<WorkerStats> {
        Arc::clone(&self.stats)
    }

    fn breaker(&self) -> MutexGuard<'_, CircuitBreaker> {
        // A poisoned breaker is still a valid state machine; keep going.
        self.breaker.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Call once at startup: jobs left `Running` by a dead process go back to the queue.
    pub async fn recover(&self) -> Result<u64, AppError> {
        let n = self
            .ports
            .jobs
            .recover_orphans(self.ports.clock.now())
            .await?;
        if n > 0 {
            tracing::warn!(count = n, "re-queued jobs orphaned by a previous run");
        }
        Ok(n)
    }

    pub async fn run_once(&self) -> Result<Tick, AppError> {
        let now = self.ports.clock.now();
        if !self.breaker().permits(now) {
            return Ok(Tick::BreakerOpen);
        }
        let Some(mut job) = self.ports.jobs.claim_next(now).await? else {
            return Ok(Tick::Idle);
        };
        self.breaker().begin(now);
        let outcome = self.process(&mut job).await?;
        Ok(Tick::Processed {
            job: job.id,
            outcome,
        })
    }

    async fn process(&self, job: &mut Job) -> Result<Outcome, AppError> {
        self.log(
            job,
            LogLevel::Info,
            format!("{ATTEMPT_START_PREFIX} {}", job.attempt + 1),
        )
        .await;
        if job.force {
            self.log(job, LogLevel::Info, FORCED_NOTE.to_owned()).await;
        }
        let started = self.ports.clock.now();
        let result = self.download_and_publish(job).await;
        let took = fanfold_domain::timestamp::since(self.ports.clock.now(), started);
        match result {
            Ok(done) => self.on_success(job, done, took).await,
            Err(err) => self.on_failure(job, err, took).await,
        }
    }

    async fn download_and_publish(&self, job: &Job) -> Result<Published, DownloadError> {
        let internal = |message: String| DownloadError {
            kind: FailureKind::Transient,
            message,
            log: String::new(),
        };
        let record = self
            .ports
            .catalog
            .get(job.work)
            .await
            .map_err(|e| internal(e.to_string()))?;
        let stored = match &record {
            Some(r)
                if self
                    .ports
                    .library
                    .exists(&r.file_name)
                    .await
                    .unwrap_or(false) =>
            {
                Some(self.ports.library.path_of(&r.file_name))
            }
            _ => None,
        };
        let updated = stored.is_some();
        // A forced retry replaces the stored book instead of updating it: it may be the one
        // that was downloaded wrong. It keeps its file name (below) for the e-reader.
        let existing = if job.force { None } else { stored };

        let book = self
            .ports
            .downloader
            .download(&DownloadRequest {
                work: job.work,
                existing,
            })
            .await?;

        // An update that found nothing new: leave the library file alone.
        if !book.changed && updated {
            self.ports.downloader.release(&book).await;
            let title = record.map_or(book.title, |r| r.title);
            return Ok(Published {
                title,
                updated: false,
                changed: false,
                log: book.log,
            });
        }

        // The name is fixed on first download and then reused, so the e-reader's
        // reading position survives title changes.
        let file_name = record.as_ref().map_or_else(
            || book_file_name(&book.title, job.work),
            |r| r.file_name.clone(),
        );
        let published = self.ports.library.publish(&book.path, &file_name).await;
        // Scratch space goes whether or not publishing worked: a failed publish is
        // retried with a fresh download.
        self.ports.downloader.release(&book).await;
        published.map_err(|e| internal(e.to_string()))?;
        self.ports
            .catalog
            .upsert(&BookRecord {
                work: job.work,
                file_name,
                title: book.title.clone(),
                updated_at: self.ports.clock.now(),
            })
            .await
            .map_err(|e| internal(e.to_string()))?;
        Ok(Published {
            title: book.title,
            updated,
            changed: true,
            log: book.log,
        })
    }

    async fn on_success(
        &self,
        job: &mut Job,
        done: Published,
        took: std::time::Duration,
    ) -> Result<Outcome, AppError> {
        let event = self.breaker().record_success();
        self.stats
            .record_success(done.updated, done.changed, took, self.ports.clock.now());
        self.keep_output(job, LogLevel::Info, &done.log).await;
        job.succeed(self.ports.clock.now())?;
        self.ports.jobs.save(job).await?;
        if done.changed {
            let verb = if done.updated {
                "updated"
            } else {
                "downloaded"
            };
            self.log(job, LogLevel::Info, format!("{verb} \"{}\"", done.title))
                .await;
            self.announce(&NotifyEvent::Downloaded {
                work: job.work,
                title: done.title,
                updated: done.updated,
            })
            .await;
        } else {
            let line = format!("\"{}\" is already up to date", done.title);
            self.log(job, LogLevel::Info, line).await;
        }
        if event == BreakerEvent::Recovered {
            self.announce(&NotifyEvent::SiteRecovered).await;
        }
        Ok(Outcome::Succeeded)
    }

    async fn on_failure(
        &self,
        job: &mut Job,
        err: DownloadError,
        took: std::time::Duration,
    ) -> Result<Outcome, AppError> {
        let now = self.ports.clock.now();
        let event = if err.kind.is_site_wide() {
            self.breaker().record_failure(now)
        } else {
            // The site answered (e.g. "no such work"): it is healthy.
            self.breaker().record_success()
        };
        self.keep_output(job, LogLevel::Warn, &err.log).await;
        let resolution = job.fail(
            now,
            err.kind,
            err.message.clone(),
            &self.retry,
            self.ports.jitter.unit(),
        )?;
        self.ports.jobs.save(job).await?;
        self.stats
            .record_failure(err.kind, resolution == Resolution::GaveUp, took);
        let line = match resolution {
            Resolution::Retrying { at } => {
                format!("failed ({}): {}; retrying at {at}", err.kind, err.message)
            }
            Resolution::GaveUp => format!("failed ({}): {}; giving up", err.kind, err.message),
        };
        self.log(job, LogLevel::Error, line).await;
        if event == BreakerEvent::Tripped {
            self.stats.record_breaker_trip();
            self.announce(&NotifyEvent::SiteDegraded { kind: err.kind })
                .await;
        }
        if resolution == Resolution::GaveUp {
            self.announce(&NotifyEvent::GaveUp {
                work: job.work,
                kind: err.kind,
                message: err.message,
            })
            .await;
        }
        if event == BreakerEvent::Recovered {
            self.announce(&NotifyEvent::SiteRecovered).await;
        }
        Ok(Outcome::Failed(resolution))
    }

    /// Job logs are diagnostics: failing to write one must never fail the job.
    async fn log(&self, job: &Job, level: LogLevel, message: String) {
        let entry = LogEntry {
            at: self.ports.clock.now(),
            attempt: job.attempt,
            level,
            message,
        };
        if let Err(e) = self.ports.jobs.append_log(job.id, &entry).await {
            tracing::warn!(job = %job.id, error = %e, "could not write job log");
        }
    }

    async fn keep_output(&self, job: &Job, level: LogLevel, output: &str) {
        let output = output.trim();
        if output.is_empty() {
            return;
        }
        let clipped: String = output.chars().take(MAX_LOG_CHARS).collect();
        self.log(job, level, clipped).await;
    }

    /// Notifications are best-effort: a Discord outage must not affect downloads.
    async fn announce(&self, event: &NotifyEvent) {
        if let Err(e) = self.ports.notifier.notify(event).await {
            tracing::warn!(error = %e, "notification failed");
        }
    }
}

impl SiteStatusSource for Worker {
    fn site_status(&self) -> SiteStatus {
        use fanfold_domain::breaker::BreakerState;
        // Copy the state out so the lock is released before matching.
        let state = self.breaker().state();
        match state {
            BreakerState::Closed { .. } => SiteStatus::Healthy,
            BreakerState::Open { until, .. } => SiteStatus::Degraded { retry_at: until },
            BreakerState::HalfOpen { .. } => SiteStatus::Probing,
        }
    }
}
