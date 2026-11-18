//! Use case: take one due job, download it, publish it, record the outcome.

use std::{
    fmt,
    sync::{Arc, Mutex, MutexGuard, PoisonError},
};

use fanfold_domain::{
    breaker::{BreakerConfig, BreakerEvent, CircuitBreaker},
    failure::FailureKind,
    job::{Job, JobId, JobSource, Resolution, Target},
    library::{series_book_path, series_folder, single_book_path},
    metadata::FollowSource,
    retry::RetryPolicy,
    work::{SeriesId, WorkId},
};

use crate::{
    error::AppError,
    ports::{
        BookCatalog, BookRecord, Clock, DownloadDetails, DownloadError, DownloadRequest,
        DownloadedBook, Downloader, EnqueueOutcome, Jitter, JobRepository, LibraryStore, LogEntry,
        LogLevel, MetadataStore, Notifier, NotifyEvent, SiteStatus, SiteStatusSource,
    },
    stats::WorkerStats,
};

/// Everything the worker talks to.
#[derive(Clone)]
pub struct Ports {
    pub jobs: Arc<dyn JobRepository>,
    pub catalog: Arc<dyn BookCatalog>,
    pub metadata: Arc<dyn MetadataStore>,
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
    /// When the current site trouble began (the breaker tripped), for the recovery notice.
    down_since: Mutex<Option<fanfold_domain::Timestamp>>,
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
    details: Option<Box<DownloadDetails>>,
}

/// A series that was listed, and what that queued.
struct Listed {
    name: String,
    works: usize,
    queued: usize,
    already_active: usize,
}

/// Every download attempt begins with a job-log line starting with this text; the
/// UI uses it to split a job's log into runs.
pub const ATTEMPT_START_PREFIX: &str = "starting attempt";

/// Logged at the start of a forced run.
pub const FORCED_NOTE: &str =
    "forced: downloading from scratch; the stored book is replaced only if this succeeds";

/// Logged when a download found something about the work that changed.
pub const METADATA_CHANGED_NOTE: &str = "the work's details changed: recorded a new version";

const MAX_LOG_CHARS: usize = 20_000;

/// One chapter seen twice: AO3's chapter id when both have one, else number and title.
fn same_chapter(
    a: &fanfold_domain::metadata::Chapter,
    b: &fanfold_domain::metadata::Chapter,
) -> bool {
    match (a.ao3_id, b.ao3_id) {
        (Some(x), Some(y)) => x == y,
        _ => a.number == b.number && a.title == b.title,
    }
}

/// A failure of our own storage, not of the site: retried like a transient error.
const fn internal(message: String) -> DownloadError {
    DownloadError {
        kind: FailureKind::Transient,
        message,
        log: String::new(),
    }
}

impl Worker {
    #[must_use]
    pub fn new(ports: Ports, config: WorkerConfig) -> Self {
        Self {
            ports,
            retry: config.retry,
            breaker: Mutex::new(CircuitBreaker::new(config.breaker)),
            stats: Arc::new(WorkerStats::new()),
            down_since: Mutex::new(None),
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
        let started = self.ports.clock.now();
        match job.target {
            Target::Work(work) => {
                if job.force {
                    self.log(job, LogLevel::Info, FORCED_NOTE.to_owned()).await;
                }
                let result = self.download_and_publish(job, work).await;
                let took = fanfold_domain::timestamp::since(self.ports.clock.now(), started);
                match result {
                    Ok(done) => self.on_success(job, work, done, took).await,
                    Err(err) => self.on_failure(job, err, took).await,
                }
            }
            Target::Series(series) => {
                let result = self.list_and_queue(job, series).await;
                let took = fanfold_domain::timestamp::since(self.ports.clock.now(), started);
                match result {
                    Ok(listed) => self.on_listed(job, listed).await,
                    Err(err) => self.on_failure(job, err, took).await,
                }
            }
        }
    }

    async fn download_and_publish(
        &self,
        job: &Job,
        work: WorkId,
    ) -> Result<Published, DownloadError> {
        let record = self
            .ports
            .catalog
            .get(work)
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
        // that was downloaded wrong. It keeps its path (below) for the e-reader.
        let existing = if job.force { None } else { stored };

        let book = self
            .ports
            .downloader
            .download(&DownloadRequest { work, existing })
            .await?;

        // The chapters recorded before this download tell what an update added.
        let previous = self.ports.metadata.work_state(work).await.ok().flatten();
        // What the site said is recorded first, even when the book itself did not change.
        if let Err(err) = self.record_metadata(job, &book).await {
            self.ports.downloader.release(&book).await;
            return Err(err);
        }

        // An update that found nothing new: leave the library file alone.
        if !book.changed && updated {
            self.ports.downloader.release(&book).await;
            let title = record.map_or(book.title, |r| r.title);
            return Ok(Published {
                title,
                updated: false,
                changed: false,
                log: book.log,
                details: None,
            });
        }

        // The path is fixed on first download and then reused, so the e-reader's reading
        // position survives title changes, renamed series and new co-authors.
        let file_name = match &record {
            Some(r) => r.file_name.clone(),
            None => match self.choose_path(&book, work).await {
                Ok(path) => path,
                Err(err) => {
                    self.ports.downloader.release(&book).await;
                    return Err(err);
                }
            },
        };
        let published = self.ports.library.publish(&book.path, &file_name).await;
        // Scratch space goes whether or not publishing worked: a failed publish is
        // retried with a fresh download.
        self.ports.downloader.release(&book).await;
        published.map_err(|e| internal(e.to_string()))?;
        self.ports
            .catalog
            .upsert(&BookRecord {
                work,
                file_name: file_name.clone(),
                title: book.title.clone(),
                updated_at: self.ports.clock.now(),
            })
            .await
            .map_err(|e| internal(e.to_string()))?;
        let details = book.metadata.map(|metadata| {
            let new_chapters = match (&previous, updated) {
                (Some(before), true) => metadata
                    .chapters
                    .iter()
                    .filter(|c| !before.chapters.iter().any(|old| same_chapter(old, c)))
                    .cloned()
                    .collect(),
                _ => Vec::new(),
            };
            Box::new(DownloadDetails {
                metadata,
                previous,
                new_chapters,
                path: file_name,
            })
        });
        Ok(Published {
            title: book.title,
            updated,
            changed: true,
            log: book.log,
            details,
        })
    }

    async fn record_metadata(&self, job: &Job, book: &DownloadedBook) -> Result<(), DownloadError> {
        let Some(metadata) = &book.metadata else {
            return Ok(());
        };
        let recorded = self
            .ports
            .metadata
            .record_work(metadata, self.ports.clock.now(), Some(job.id))
            .await
            .map_err(|e| internal(format!("cannot record the work's metadata: {e}")))?;
        if recorded.new_version {
            self.log(job, LogLevel::Info, METADATA_CHANGED_NOTE.to_owned())
                .await;
        }
        Ok(())
    }

    /// Where a new book goes: under the first followed series it belongs to (in the order
    /// AO3 lists them), otherwise under its byline.
    async fn choose_path(
        &self,
        book: &DownloadedBook,
        work: WorkId,
    ) -> Result<String, DownloadError> {
        let Some(metadata) = &book.metadata else {
            return Ok(single_book_path("", &book.title, work));
        };
        let title = if metadata.facts.title.trim().is_empty() {
            &book.title
        } else {
            &metadata.facts.title
        };
        let candidates: Vec<SeriesId> = metadata.series.iter().map(|p| p.id).collect();
        let followed = self
            .ports
            .metadata
            .followed(&candidates)
            .await
            .map_err(|e| internal(e.to_string()))?;
        let Some(part) = followed
            .first()
            .and_then(|id| metadata.series.iter().find(|p| p.id == *id))
        else {
            return Ok(single_book_path(&metadata.facts.byline, title, work));
        };
        let folder = self
            .ports
            .metadata
            .claim_series_folder(part.id, &series_folder(&part.name, part.id))
            .await
            .map_err(|e| internal(e.to_string()))?;
        Ok(series_book_path(&folder, part.part, title, work))
    }

    /// List a series, follow it, and queue every work in it.
    async fn list_and_queue(&self, job: &Job, series: SeriesId) -> Result<Listed, DownloadError> {
        let listing = self.ports.downloader.list_series(series).await?;
        if listing.facts.works.is_empty() {
            // An error page in place of the series page lists nothing; never "succeed" on it.
            return Err(DownloadError {
                kind: FailureKind::SiteUnavailable,
                message: "the series page listed no works".into(),
                log: String::new(),
            });
        }
        let now = self.ports.clock.now();
        let store = |e: crate::error::RepoError| internal(e.to_string());
        self.ports
            .metadata
            .record_series(&listing, now, Some(job.id))
            .await
            .map_err(store)?;
        let by = match job.source {
            JobSource::Mail => FollowSource::Mail,
            JobSource::Manual => FollowSource::Manual,
        };
        self.ports
            .metadata
            .follow_series(series, by, now)
            .await
            .map_err(store)?;
        self.ports
            .metadata
            .claim_series_folder(series, &series_folder(&listing.facts.name, series))
            .await
            .map_err(store)?;
        let mut listed = Listed {
            name: listing.facts.name.clone(),
            works: listing.facts.works.len(),
            queued: 0,
            already_active: 0,
        };
        for work in &listing.facts.works {
            match self
                .ports
                .jobs
                .enqueue(Target::Work(*work), job.source, now)
                .await
                .map_err(store)?
            {
                EnqueueOutcome::Created(_) => listed.queued += 1,
                EnqueueOutcome::AlreadyActive(_) => listed.already_active += 1,
            }
        }
        Ok(listed)
    }

    async fn on_listed(&self, job: &mut Job, listed: Listed) -> Result<Outcome, AppError> {
        let event = self.breaker().record_success();
        job.succeed(self.ports.clock.now())?;
        self.ports.jobs.save(job).await?;
        let line = format!(
            "listed \"{}\": {} works; queued {}, {} already queued",
            listed.name, listed.works, listed.queued, listed.already_active
        );
        self.log(job, LogLevel::Info, line).await;
        if let Target::Series(series) = job.target {
            self.announce(&NotifyEvent::SeriesListed {
                job: job.id,
                series,
                name: listed.name,
                works: listed.works,
                queued: listed.queued,
            })
            .await;
        }
        if event == BreakerEvent::Recovered {
            self.announce_recovery().await;
        }
        Ok(Outcome::Succeeded)
    }

    async fn on_success(
        &self,
        job: &mut Job,
        work: WorkId,
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
                job: job.id,
                work,
                title: done.title,
                updated: done.updated,
                details: done.details,
            })
            .await;
        } else {
            let line = format!("\"{}\" is already up to date", done.title);
            self.log(job, LogLevel::Info, line).await;
        }
        if event == BreakerEvent::Recovered {
            self.announce_recovery().await;
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
            self.announce_degraded(err.kind, &err.message, now).await;
        }
        if resolution == Resolution::GaveUp {
            let title = self.target_name(job.target).await;
            self.announce(&NotifyEvent::GaveUp {
                job: job.id,
                target: job.target,
                title,
                kind: err.kind,
                message: err.message,
                attempts: job.attempt,
                max_attempts: self.retry.max_attempts,
            })
            .await;
        }
        if event == BreakerEvent::Recovered {
            self.announce_recovery().await;
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

    async fn announce_degraded(
        &self,
        kind: FailureKind,
        message: &str,
        now: fanfold_domain::Timestamp,
    ) {
        use fanfold_domain::breaker::BreakerState;
        self.down_since
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get_or_insert(now);
        let state = self.breaker().state();
        let retry_at = match state {
            BreakerState::Open { until, .. } => until,
            _ => now,
        };
        let waiting = self
            .ports
            .jobs
            .counts()
            .await
            .ok()
            .map(|c| c.queued + c.retrying);
        self.announce(&NotifyEvent::SiteDegraded {
            kind,
            message: message.to_owned(),
            retry_at,
            waiting,
        })
        .await;
    }

    async fn announce_recovery(&self) {
        let down_since = self
            .down_since
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
        self.announce(&NotifyEvent::SiteRecovered {
            down_since,
            at: self.ports.clock.now(),
        })
        .await;
    }

    /// A work's book title, or a series' name, when known. Best effort.
    async fn target_name(&self, target: Target) -> Option<String> {
        match target {
            Target::Work(work) => self
                .ports
                .catalog
                .get(work)
                .await
                .ok()
                .flatten()
                .map(|b| b.title),
            Target::Series(series) => self
                .ports
                .metadata
                .series_names(&[series])
                .await
                .ok()
                .and_then(|mut names| names.remove(&series)),
        }
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
