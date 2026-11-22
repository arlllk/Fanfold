//! Ports: everything the application needs from the world outside it.
//!
//! Every trait here can be mocked with `mockall` (feature `mocks`), and has an
//! in-memory fake in `fanfold-testkit`. Real implementations live in the
//! `fanfold-adapter-*` crates.

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
};

use async_trait::async_trait;
use fanfold_domain::{
    Timestamp,
    failure::FailureKind,
    job::{Job, JobId, JobSource, Target},
    metadata::{
        Chapter, FollowSource, SeriesMetadata, SeriesPart, Tag, WorkFacts, WorkMetadata, WorkState,
        WorkStats, WorkVersionChanges,
    },
    work::{SeriesId, WorkId},
};
use tokio::sync::broadcast;

use crate::error::{MailError, NotifyError, RepoError, StoreError};

// ---------------------------------------------------------------- time, chance

/// The only way application code learns the time.
#[cfg_attr(any(test, feature = "mocks"), mockall::automock)]
pub trait Clock: Send + Sync {
    fn now(&self) -> Timestamp;
}

/// The only source of randomness: a unit value in `[0, 1)` for retry jitter.
#[cfg_attr(any(test, feature = "mocks"), mockall::automock)]
pub trait Jitter: Send + Sync {
    fn unit(&self) -> f64;
}

// ------------------------------------------------------------------------ mail

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MailMessage {
    pub id: String,
    pub from: String,
    pub subject: String,
    pub text: String,
    pub html: String,
}

/// Opaque resume token (for example a JMAP state string).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Checkpoint(pub String);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MailBatch {
    pub messages: Vec<MailMessage>,
    pub checkpoint: Checkpoint,
}

/// Incoming mail with at-least-once semantics.
///
/// A checkpoint is only committed after every message in the batch has been
/// handled, so a crash replays the batch rather than losing it. Handling must
/// therefore be idempotent.
#[cfg_attr(any(test, feature = "mocks"), mockall::automock)]
#[async_trait]
pub trait MailSource: Send + Sync {
    async fn fetch_new(&self) -> Result<MailBatch, MailError>;
    async fn commit(&self, checkpoint: &Checkpoint) -> Result<(), MailError>;
}

// ------------------------------------------------------------------- job queue

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogLevel {
    Info,
    Warn,
    Error,
}

impl LogLevel {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Info => "info",
            Self::Warn => "warn",
            Self::Error => "error",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogEntry {
    pub at: Timestamp,
    pub attempt: u32,
    pub level: LogLevel,
    pub message: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EnqueueOutcome {
    Created(Job),
    /// A queued or running job for this target already exists.
    AlreadyActive(JobId),
}

/// Which jobs a listing returns.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JobFilter {
    All,
    /// Queued or running.
    Active,
    /// Queued again after a failure: has an error and is waiting for its next attempt.
    Retrying,
    /// Failed and not resolved: no later job for the same target has succeeded.
    Failed,
    Succeeded,
    /// Succeeded, failed or cancelled.
    Finished,
}

impl JobFilter {
    pub const ALL: [Self; 6] = [
        Self::All,
        Self::Active,
        Self::Retrying,
        Self::Failed,
        Self::Succeeded,
        Self::Finished,
    ];

    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::All => "all",
            Self::Active => "active",
            Self::Retrying => "retrying",
            Self::Failed => "failed",
            Self::Succeeded => "succeeded",
            Self::Finished => "finished",
        }
    }

    #[must_use]
    pub fn parse(s: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|f| f.as_str() == s)
    }

    /// The single definition of what each filter means. Adapters implement it in
    /// their own query language and the contract suite checks they agree.
    #[must_use]
    pub fn matches(self, job: &Job) -> bool {
        use fanfold_domain::job::JobState as S;
        match self {
            Self::All => true,
            Self::Active => job.state.is_active(),
            Self::Retrying => job.state == S::Queued && job.last_error.is_some(),
            Self::Failed => job.needs_attention(),
            Self::Succeeded => job.state == S::Succeeded,
            Self::Finished => matches!(job.state, S::Succeeded | S::Failed | S::Cancelled),
        }
    }
}

/// Job totals for the dashboard and metrics.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct JobCounts {
    /// Queued and never failed.
    pub queued: u64,
    pub running: u64,
    /// Queued again after a failure.
    pub retrying: u64,
    pub succeeded: u64,
    /// Failed and not resolved since.
    pub failed: u64,
    /// Failed, then fixed by a later job for the same target.
    pub resolved: u64,
    pub cancelled: u64,
}

/// Durable job queue and per-job log.
///
/// Contract (checked by `fanfold_testkit::contracts::job_repository`):
/// * at most one *active* (queued or running) job per work;
/// * `claim_next` atomically moves exactly one due `Queued` job to `Running`,
///   oldest `next_attempt_at` first, and is safe under concurrent callers;
/// * `save` overwrites a job's mutable fields, and fails with
///   [`RepoError::Conflict`] if that would leave two active jobs for one target;
/// * `resolved_by` belongs to the repository: saving a `Succeeded` job sets it on every
///   unresolved `Failed` job for the same target, and `save` keeps it only while the job
///   it is saving stays `Failed` (whatever the given job carries).
#[cfg_attr(any(test, feature = "mocks"), mockall::automock)]
#[async_trait]
pub trait JobRepository: Send + Sync {
    async fn enqueue(
        &self,
        target: Target,
        source: JobSource,
        now: Timestamp,
    ) -> Result<EnqueueOutcome, RepoError>;

    async fn claim_next(&self, now: Timestamp) -> Result<Option<Job>, RepoError>;

    async fn save(&self, job: &Job) -> Result<(), RepoError>;

    async fn get(&self, id: JobId) -> Result<Option<Job>, RepoError>;

    /// Matching jobs, most recently updated first.
    async fn list(&self, filter: JobFilter, limit: u32, offset: u32)
    -> Result<Vec<Job>, RepoError>;

    async fn counts(&self) -> Result<JobCounts, RepoError>;

    /// Re-queue jobs left `Running` by a process that died. Returns how many.
    async fn recover_orphans(&self, now: Timestamp) -> Result<u64, RepoError>;

    async fn append_log(&self, job: JobId, entry: &LogEntry) -> Result<(), RepoError>;

    /// Oldest first.
    async fn logs(&self, job: JobId) -> Result<Vec<LogEntry>, RepoError>;
}

// --------------------------------------------------------------------- library

/// What we remember about a book already in the library. The file name is chosen
/// once and never changes: e-readers key reading progress on the path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BookRecord {
    pub work: WorkId,
    pub file_name: String,
    pub title: String,
    pub updated_at: Timestamp,
}

#[cfg_attr(any(test, feature = "mocks"), mockall::automock)]
#[async_trait]
pub trait BookCatalog: Send + Sync {
    async fn get(&self, work: WorkId) -> Result<Option<BookRecord>, RepoError>;
    async fn upsert(&self, record: &BookRecord) -> Result<(), RepoError>;
    /// Known titles for these works; works without a book are simply absent.
    async fn titles(&self, works: &[WorkId]) -> Result<HashMap<WorkId, String>, RepoError>;
    /// Forget a work's book. Returns whether there was one.
    async fn remove(&self, work: WorkId) -> Result<bool, RepoError>;
}

// --------------------------------------------------------------------- library

/// Which books a library listing shows, by completion.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub enum LibraryStatus {
    #[default]
    All,
    /// Every chapter the author planned is posted.
    Complete,
    /// Known, and not complete (including works whose total AO3 shows as "?").
    InProgress,
}

impl LibraryStatus {
    pub const ALL: [Self; 3] = [Self::All, Self::Complete, Self::InProgress];

    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::All => "all",
            Self::Complete => "complete",
            Self::InProgress => "in_progress",
        }
    }

    #[must_use]
    pub fn parse(raw: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|s| s.as_str() == raw)
    }
}

/// The order of a library listing. Every order ends with the work id, so it is total.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub enum LibrarySort {
    /// AO3's "updated" date, newest first (works without one last), then the newest book.
    #[default]
    Updated,
    /// By title, A to Z (lower-cased, by code point).
    Title,
    /// By byline, A to Z (lower-cased, by code point), then title.
    Author,
    /// Longest first.
    Words,
}

impl LibrarySort {
    pub const ALL: [Self; 4] = [Self::Updated, Self::Title, Self::Author, Self::Words];

    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Updated => "updated",
            Self::Title => "title",
            Self::Author => "author",
            Self::Words => "words",
        }
    }

    #[must_use]
    pub fn parse(raw: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|s| s.as_str() == raw)
    }
}

/// One page of the library to list.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LibraryQuery {
    /// Case-insensitive substring of the title, the byline, any current tag, or a series
    /// name. Blank means everything.
    pub search: Option<String>,
    pub status: LibraryStatus,
    pub sort: LibrarySort,
    pub limit: u32,
    pub offset: u32,
}

/// One book in the library, with what AO3 last said about its work.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LibraryEntry {
    pub book: BookRecord,
    /// The latest recorded facts; `None` for a book whose work was never observed.
    pub facts: Option<WorkFacts>,
    /// The work's current fandom, relationship and warning tags, in AO3's order.
    pub tags: Vec<Tag>,
    pub series: Vec<SeriesPart>,
    /// The latest counters.
    pub stats: WorkStats,
    pub last_checked: Option<Timestamp>,
}

impl LibraryEntry {
    /// The title to show: AO3's latest, else the one the book was saved with.
    #[must_use]
    pub fn title(&self) -> &str {
        self.facts.as_ref().map_or(&self.book.title, |f| &f.title)
    }

    /// Whether every planned chapter is posted (`None`: never observed).
    #[must_use]
    pub fn complete(&self) -> Option<bool> {
        self.facts
            .as_ref()
            .map(|f| f.chapters_total == Some(f.chapters_posted))
    }
}

/// Totals over the whole library, whatever the query.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct LibraryTotals {
    pub books: u64,
    pub words: u64,
    pub complete: u64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LibraryPage {
    pub entries: Vec<LibraryEntry>,
    /// How many books match the query (all pages).
    pub matching: u64,
    pub totals: LibraryTotals,
}

/// The library as a list: every book, with its work's latest metadata.
#[cfg_attr(any(test, feature = "mocks"), mockall::automock)]
#[async_trait]
pub trait LibraryCatalog: Send + Sync {
    async fn library(&self, query: &LibraryQuery) -> Result<LibraryPage, RepoError>;
    /// One book, as listed (`None` when the work has no book).
    async fn entry(&self, work: WorkId) -> Result<Option<LibraryEntry>, RepoError>;
}

/// The folder the e-reader syncs from.
#[cfg_attr(any(test, feature = "mocks"), mockall::automock)]
#[async_trait]
pub trait LibraryStore: Send + Sync {
    fn path_of(&self, file_name: &str) -> PathBuf;

    async fn exists(&self, file_name: &str) -> Result<bool, StoreError>;

    /// Atomically place `source` at `file_name`, replacing any previous file, so a
    /// syncing reader never observes a partial book. Consumes `source`.
    async fn publish(&self, source: &Path, file_name: &str) -> Result<(), StoreError>;

    /// Delete a book, and the folders it leaves empty. Returns whether the file existed.
    async fn remove(&self, file_name: &str) -> Result<bool, StoreError>;
}

// -------------------------------------------------------------------- metadata

/// Result of recording an observation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Recorded {
    /// A new version was written: something changed since the last observation.
    pub new_version: bool,
}

/// The history of what AO3 said about works and series, and which series are followed.
/// See `fanfold_domain::metadata` for the version-and-span model.
#[cfg_attr(any(test, feature = "mocks"), mockall::automock)]
#[async_trait]
pub trait MetadataStore: Send + Sync {
    /// Record one observation of a work, atomically: always its statistics and "last
    /// checked" time; a new version and span changes only when something changed.
    async fn record_work(
        &self,
        metadata: &WorkMetadata,
        seen_at: Timestamp,
        job: Option<JobId>,
    ) -> Result<Recorded, RepoError>;

    /// Record one listing of a series; a new series version only when it changed.
    async fn record_series(
        &self,
        metadata: &SeriesMetadata,
        seen_at: Timestamp,
        job: Option<JobId>,
    ) -> Result<Recorded, RepoError>;

    /// The latest recorded version and open spans of a work.
    async fn work_state(&self, work: WorkId) -> Result<Option<WorkState>, RepoError>;

    /// Every recorded version of a work with what it changed, oldest first (empty for a
    /// work never observed). Lists keep the order the work's page showed them in.
    async fn work_history(&self, work: WorkId) -> Result<Vec<WorkVersionChanges>, RepoError>;

    /// Start following a series. Idempotent: the first follow's time and source are kept.
    async fn follow_series(
        &self,
        series: SeriesId,
        by: FollowSource,
        at: Timestamp,
    ) -> Result<(), RepoError>;

    /// Stop following a series. Returns whether it was followed.
    async fn unfollow_series(&self, series: SeriesId) -> Result<bool, RepoError>;

    /// The works a series contains, as far as is known: those whose page currently names
    /// it, and those its latest listing showed.
    async fn series_members(&self, series: SeriesId) -> Result<Vec<WorkId>, RepoError>;

    /// The followed series among `candidates`, in the order given.
    async fn followed(&self, candidates: &[SeriesId]) -> Result<Vec<SeriesId>, RepoError>;

    /// The series' library folder, chosen once: the stored one if there is one, otherwise
    /// `proposed`, which is stored.
    async fn claim_series_folder(
        &self,
        series: SeriesId,
        proposed: &str,
    ) -> Result<String, RepoError>;

    /// Best known names: the latest listing's, else the name a work's page showed.
    async fn series_names(
        &self,
        series: &[SeriesId],
    ) -> Result<HashMap<SeriesId, String>, RepoError>;
}

// ------------------------------------------------------------------ downloader

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DownloadRequest {
    pub work: WorkId,
    /// Existing library file to update incrementally, when there is one.
    pub existing: Option<PathBuf>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DownloadedBook {
    /// Finished EPUB in the downloader's scratch space.
    pub path: PathBuf,
    pub title: String,
    /// `false` when an update found nothing new: the file is identical to the one
    /// handed in, so there is nothing to publish or announce.
    pub changed: bool,
    /// Diagnostic output worth keeping in the job log.
    pub log: String,
    /// What the site said about the work during this download.
    pub metadata: Option<WorkMetadata>,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{kind}: {message}")]
pub struct DownloadError {
    pub kind: FailureKind,
    pub message: String,
    pub log: String,
}

#[cfg_attr(any(test, feature = "mocks"), mockall::automock)]
#[async_trait]
pub trait Downloader: Send + Sync {
    async fn download(&self, request: &DownloadRequest) -> Result<DownloadedBook, DownloadError>;

    /// List a series: its facts and its works in order. Fails like a download does.
    async fn list_series(&self, series: SeriesId) -> Result<SeriesMetadata, DownloadError>;

    /// Discard whatever scratch space backs `book`. Called exactly once for every
    /// book `download` returned, after it was published or abandoned. Best effort:
    /// it cannot fail, only log. Whoever creates scratch space removes it.
    async fn release(&self, book: &DownloadedBook);
}

// ---------------------------------------------------------------- book finisher

/// What Fanfold knows about a book's work beyond what the downloader wrote into it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BookExtras {
    /// What the site said during this download.
    pub metadata: WorkMetadata,
    /// The work's recorded versions, oldest first (this download's included).
    pub history: Vec<WorkVersionChanges>,
    /// The series the book is grouped under when a reader takes only one: the followed
    /// series the library files it under, else the first the work's page names.
    pub main_series: Option<SeriesId>,
    /// When this download happened (the counters are as of then).
    pub fetched_at: Timestamp,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("cannot finish the book: {0}")]
pub struct FinishError(pub String);

/// Adds what Fanfold knows to a downloaded book before it is published.
///
/// Structured metadata and a cover. Must be idempotent: an update hands the
/// downloader the finished book, and the downloader may carry parts of it over.
#[cfg_attr(any(test, feature = "mocks"), mockall::automock)]
#[async_trait]
pub trait BookFinisher: Send + Sync {
    /// Rewrite the EPUB at `book` in place. On error the file is left as it was.
    async fn finish(&self, book: &Path, extras: &BookExtras) -> Result<(), FinishError>;

    /// Whether `book` already carries what this version of the finisher adds; `false` for a
    /// book finished by an older version, never finished, or unreadable.
    async fn is_current(&self, book: &Path) -> bool;
}

// --------------------------------------------------------------------- cursors

/// A small durable key/value store for resume tokens, such as the mail cursor.
#[cfg_attr(any(test, feature = "mocks"), mockall::automock)]
#[async_trait]
pub trait CursorStore: Send + Sync {
    async fn get(&self, name: &str) -> Result<Option<String>, RepoError>;
    async fn set(&self, name: &str, value: &str) -> Result<(), RepoError>;
}

// ---------------------------------------------------------------------- events

/// Something changed that a live view may want to refetch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Event {
    JobChanged(JobId),
    LogAppended(JobId),
    /// Events may have been missed (reconnect, slow consumer): refetch everything.
    Resync,
}

/// A stream of change notifications. Delivery is best effort; consumers refetch
/// state on [`Event::Resync`] instead of relying on seeing every event.
#[cfg_attr(any(test, feature = "mocks"), mockall::automock)]
pub trait EventSource: Send + Sync {
    fn subscribe(&self) -> broadcast::Receiver<Event>;
}

/// Can the service do its job right now (is the database reachable)?
#[cfg_attr(any(test, feature = "mocks"), mockall::automock)]
#[async_trait]
pub trait Readiness: Send + Sync {
    async fn check(&self) -> Result<(), RepoError>;
}

/// How the site we download from is doing, as seen by the circuit breaker.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SiteStatus {
    Healthy,
    /// Downloads are paused; the next probe is due at `retry_at`.
    Degraded {
        retry_at: Timestamp,
    },
    /// A single probe request is in flight.
    Probing,
}

#[cfg_attr(any(test, feature = "mocks"), mockall::automock)]
pub trait SiteStatusSource: Send + Sync {
    fn site_status(&self) -> SiteStatus;
}

// -------------------------------------------------------------------- notifier

/// What a finished download brought, for notifications.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DownloadDetails {
    pub metadata: WorkMetadata,
    /// What was recorded about the work before this download (`None` on a first download, or
    /// after the history was started afresh): what an update changed is the difference.
    pub previous: Option<WorkState>,
    /// The chapters this update added, in order. Empty on a first download.
    pub new_chapters: Vec<Chapter>,
    /// Where the book is in the library.
    pub path: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NotifyEvent {
    Downloaded {
        job: JobId,
        work: WorkId,
        title: String,
        updated: bool,
        /// Present when the site told us about the work during the download.
        details: Option<Box<DownloadDetails>>,
    },
    /// A series was listed and its works queued.
    SeriesListed {
        job: JobId,
        series: SeriesId,
        name: String,
        works: usize,
        queued: usize,
    },
    GaveUp {
        job: JobId,
        target: Target,
        /// The work's or series' name, when known.
        title: Option<String>,
        kind: FailureKind,
        message: String,
        /// Failures that counted against the job, and how many it was allowed.
        attempts: u32,
        max_attempts: u32,
    },
    /// The circuit breaker opened: the site is having trouble.
    SiteDegraded {
        kind: FailureKind,
        /// The failure that tipped it over.
        message: String,
        /// When the next probe download may run.
        retry_at: Timestamp,
        /// Jobs waiting meanwhile, when known.
        waiting: Option<u64>,
    },
    SiteRecovered {
        /// When the trouble began, when known.
        down_since: Option<Timestamp>,
        at: Timestamp,
    },
}

#[cfg_attr(any(test, feature = "mocks"), mockall::automock)]
#[async_trait]
pub trait Notifier: Send + Sync {
    async fn notify(&self, event: &NotifyEvent) -> Result<(), NotifyError>;
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Adapters persist these strings (the database has a CHECK on them).
    #[test]
    fn log_levels_have_stable_names() {
        assert_eq!(LogLevel::Info.as_str(), "info");
        assert_eq!(LogLevel::Warn.as_str(), "warn");
        assert_eq!(LogLevel::Error.as_str(), "error");
    }

    fn job(state: fanfold_domain::job::JobState, error: Option<&str>) -> Job {
        use fanfold_domain::{
            job::{JobId, JobSource},
            work::WorkId,
        };
        let t = Timestamp::from_second(1).unwrap();
        let mut job = Job::new(JobId::new(1), WorkId::new(1).unwrap(), JobSource::Mail, t);
        job.state = state;
        job.last_error = error.map(str::to_owned);
        job
    }

    #[test]
    fn filters_have_one_definition() {
        use fanfold_domain::job::JobState::{Cancelled, Failed, Queued, Running, Succeeded};
        // state, last_error -> which filters match
        let table: [(_, Option<&str>, &[JobFilter]); 7] = [
            (Queued, None, &[JobFilter::All, JobFilter::Active]),
            (
                Queued,
                Some("503"),
                &[JobFilter::All, JobFilter::Active, JobFilter::Retrying],
            ),
            (Running, None, &[JobFilter::All, JobFilter::Active]),
            (
                Running,
                Some("old error"),
                &[JobFilter::All, JobFilter::Active],
            ),
            (
                Succeeded,
                None,
                &[JobFilter::All, JobFilter::Succeeded, JobFilter::Finished],
            ),
            (
                Failed,
                Some("x"),
                &[JobFilter::All, JobFilter::Failed, JobFilter::Finished],
            ),
            (Cancelled, None, &[JobFilter::All, JobFilter::Finished]),
        ];
        for (state, error, expected) in table {
            let j = job(state, error);
            for filter in JobFilter::ALL {
                assert_eq!(
                    filter.matches(&j),
                    expected.contains(&filter),
                    "{state} {error:?} {filter:?}"
                );
            }
        }
    }

    #[test]
    fn filter_names_roundtrip() {
        for f in JobFilter::ALL {
            assert_eq!(JobFilter::parse(f.as_str()), Some(f));
        }
        assert_eq!(JobFilter::parse("nope"), None);
    }
}
