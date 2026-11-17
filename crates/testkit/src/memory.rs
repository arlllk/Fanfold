//! Deterministic in-memory fakes of the ports.

use std::{
    collections::{BTreeMap, HashMap, VecDeque},
    path::{Path, PathBuf},
    sync::{
        Mutex, MutexGuard, PoisonError,
        atomic::{AtomicI64, Ordering},
    },
    time::Duration,
};

use async_trait::async_trait;
use fanfold_app::{
    error::{MailError, NotifyError, RepoError, StoreError},
    ports::{
        BookCatalog, BookRecord, Checkpoint, Clock, CursorStore, DownloadError, DownloadRequest,
        DownloadedBook, Downloader, EnqueueOutcome, Event, EventSource, Jitter, JobCounts,
        JobFilter, JobRepository, LibraryStore, LogEntry, MailBatch, MailSource, Notifier,
        NotifyEvent, Readiness, SessionStore,
    },
};
use fanfold_domain::{
    Timestamp,
    failure::FailureKind,
    job::{Job, JobId, JobSource, JobState},
    session::BrowserSession,
    timestamp,
    work::WorkId,
};
use tokio::sync::broadcast;

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

// ------------------------------------------------------------------ clock etc.

/// A clock that only moves when told to.
#[derive(Debug)]
pub struct FakeClock(AtomicI64);

impl FakeClock {
    #[must_use]
    pub const fn at_millis(ms: i64) -> Self {
        Self(AtomicI64::new(ms))
    }

    #[must_use]
    pub const fn at_secs(secs: i64) -> Self {
        Self::at_millis(secs * 1000)
    }

    pub fn advance(&self, by: Duration) {
        let ms = i64::try_from(by.as_millis()).unwrap_or(i64::MAX);
        self.0.fetch_add(ms, Ordering::SeqCst);
    }

    pub fn set_secs(&self, secs: i64) {
        self.0.store(secs * 1000, Ordering::SeqCst);
    }
}

impl Clock for FakeClock {
    fn now(&self) -> Timestamp {
        timestamp::from_millis(self.0.load(Ordering::SeqCst)).expect("fake clock out of range")
    }
}

/// Always returns the same jitter value.
#[derive(Debug, Clone, Copy)]
pub struct FixedJitter(pub f64);

impl Jitter for FixedJitter {
    fn unit(&self) -> f64 {
        self.0
    }
}

// ------------------------------------------------------------------- job queue

#[derive(Debug, Default)]
struct JobsState {
    next_id: i64,
    jobs: BTreeMap<JobId, Job>,
    logs: HashMap<JobId, Vec<LogEntry>>,
    books: HashMap<WorkId, BookRecord>,
    broken: Option<RepoError>,
}

/// In-memory [`JobRepository`], [`BookCatalog`], [`EventSource`] and [`Readiness`].
#[derive(Debug)]
pub struct InMemoryJobs {
    state: Mutex<JobsState>,
    events: broadcast::Sender<Event>,
}

impl Default for InMemoryJobs {
    fn default() -> Self {
        Self {
            state: Mutex::default(),
            events: broadcast::channel(256).0,
        }
    }
}

impl InMemoryJobs {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Make every subsequent call fail with `error`, or heal with `None`.
    pub fn break_with(&self, error: Option<RepoError>) {
        lock(&self.state).broken = error;
    }

    /// Snapshot of every job, by id.
    #[must_use]
    pub fn all_jobs(&self) -> Vec<Job> {
        lock(&self.state).jobs.values().cloned().collect()
    }

    fn state(&self) -> Result<MutexGuard<'_, JobsState>, RepoError> {
        let state = lock(&self.state);
        match &state.broken {
            Some(e) => Err(e.clone()),
            None => Ok(state),
        }
    }

    fn emit(&self, event: Event) {
        // Having no subscribers is fine.
        let _ = self.events.send(event);
    }
}

impl EventSource for InMemoryJobs {
    fn subscribe(&self) -> broadcast::Receiver<Event> {
        self.events.subscribe()
    }
}

#[async_trait]
impl Readiness for InMemoryJobs {
    async fn check(&self) -> Result<(), RepoError> {
        self.state().map(|_| ())
    }
}

#[async_trait]
impl JobRepository for InMemoryJobs {
    async fn enqueue(
        &self,
        work: WorkId,
        source: JobSource,
        now: Timestamp,
    ) -> Result<EnqueueOutcome, RepoError> {
        let mut state = self.state()?;
        if let Some(active) = state
            .jobs
            .values()
            .find(|j| j.work == work && j.state.is_active())
        {
            return Ok(EnqueueOutcome::AlreadyActive(active.id));
        }
        state.next_id += 1;
        let job = Job::new(JobId::new(state.next_id), work, source, now);
        state.jobs.insert(job.id, job.clone());
        drop(state);
        self.emit(Event::JobChanged(job.id));
        Ok(EnqueueOutcome::Created(job))
    }

    async fn claim_next(&self, now: Timestamp) -> Result<Option<Job>, RepoError> {
        let mut state = self.state()?;
        let due = state
            .jobs
            .values()
            .filter(|j| j.state == JobState::Queued && j.next_attempt_at <= now)
            .min_by_key(|j| (j.next_attempt_at, j.id))
            .map(|j| j.id);
        let Some(id) = due else { return Ok(None) };
        let job = state.jobs.get_mut(&id).ok_or(RepoError::NotFound(id))?;
        job.start(now)
            .map_err(|e| RepoError::Backend(e.to_string()))?;
        let claimed = job.clone();
        drop(state);
        self.emit(Event::JobChanged(id));
        Ok(Some(claimed))
    }

    async fn save(&self, job: &Job) -> Result<(), RepoError> {
        let mut state = self.state()?;
        if !state.jobs.contains_key(&job.id) {
            return Err(RepoError::NotFound(job.id));
        }
        let clashes = job.state.is_active()
            && state.jobs.values().any(|other| {
                other.id != job.id && other.work == job.work && other.state.is_active()
            });
        if clashes {
            return Err(RepoError::Conflict(format!(
                "work {} already has an active job",
                job.work
            )));
        }
        state.jobs.insert(job.id, job.clone());
        drop(state);
        self.emit(Event::JobChanged(job.id));
        Ok(())
    }

    async fn get(&self, id: JobId) -> Result<Option<Job>, RepoError> {
        Ok(self.state()?.jobs.get(&id).cloned())
    }

    async fn list(
        &self,
        filter: JobFilter,
        limit: u32,
        offset: u32,
    ) -> Result<Vec<Job>, RepoError> {
        let state = self.state()?;
        let mut jobs: Vec<Job> = state
            .jobs
            .values()
            .filter(|j| filter.matches(j))
            .cloned()
            .collect();
        jobs.sort_by_key(|j| std::cmp::Reverse((j.updated_at, j.id)));
        let skip = usize::try_from(offset).unwrap_or(usize::MAX);
        let take = usize::try_from(limit).unwrap_or(usize::MAX);
        Ok(jobs.into_iter().skip(skip).take(take).collect())
    }

    async fn counts(&self) -> Result<JobCounts, RepoError> {
        let state = self.state()?;
        let mut counts = JobCounts::default();
        for job in state.jobs.values() {
            match (job.state, job.last_error.is_some()) {
                (JobState::Queued, false) => counts.queued += 1,
                (JobState::Queued, true) => counts.retrying += 1,
                (JobState::Running, _) => counts.running += 1,
                (JobState::Succeeded, _) => counts.succeeded += 1,
                (JobState::Failed, _) => counts.failed += 1,
                (JobState::Cancelled, _) => counts.cancelled += 1,
            }
        }
        Ok(counts)
    }

    async fn recover_orphans(&self, now: Timestamp) -> Result<u64, RepoError> {
        let mut state = self.state()?;
        let mut revived = Vec::new();
        for job in state.jobs.values_mut() {
            if job.requeue_orphan(now).is_ok() {
                revived.push(job.id);
            }
        }
        drop(state);
        for id in &revived {
            self.emit(Event::JobChanged(*id));
        }
        Ok(revived.len() as u64)
    }

    async fn append_log(&self, job: JobId, entry: &LogEntry) -> Result<(), RepoError> {
        let mut state = self.state()?;
        if !state.jobs.contains_key(&job) {
            return Err(RepoError::NotFound(job));
        }
        state.logs.entry(job).or_default().push(entry.clone());
        drop(state);
        self.emit(Event::LogAppended(job));
        Ok(())
    }

    async fn logs(&self, job: JobId) -> Result<Vec<LogEntry>, RepoError> {
        Ok(self.state()?.logs.get(&job).cloned().unwrap_or_default())
    }
}

#[async_trait]
impl BookCatalog for InMemoryJobs {
    async fn get(&self, work: WorkId) -> Result<Option<BookRecord>, RepoError> {
        Ok(self.state()?.books.get(&work).cloned())
    }

    async fn upsert(&self, record: &BookRecord) -> Result<(), RepoError> {
        let mut state = self.state()?;
        match state.books.get_mut(&record.work) {
            // Mirrors the database: the file name is fixed at first insert.
            Some(existing) => {
                existing.title.clone_from(&record.title);
                existing.updated_at = record.updated_at;
            }
            None => {
                state.books.insert(record.work, record.clone());
            }
        }
        Ok(())
    }

    async fn titles(&self, works: &[WorkId]) -> Result<HashMap<WorkId, String>, RepoError> {
        let state = self.state()?;
        Ok(works
            .iter()
            .filter_map(|w| state.books.get(w).map(|b| (*w, b.title.clone())))
            .collect())
    }
}

// --------------------------------------------------------------------- cursors

/// In-memory [`CursorStore`].
#[derive(Debug, Default)]
pub struct InMemoryCursors {
    values: Mutex<HashMap<String, String>>,
    broken: Mutex<Option<RepoError>>,
}

impl InMemoryCursors {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    pub fn break_with(&self, error: Option<RepoError>) {
        *lock(&self.broken) = error;
    }
}

#[async_trait]
impl CursorStore for InMemoryCursors {
    async fn get(&self, name: &str) -> Result<Option<String>, RepoError> {
        let broken = lock(&self.broken).clone();
        if let Some(e) = broken {
            return Err(e);
        }
        Ok(lock(&self.values).get(name).cloned())
    }

    async fn set(&self, name: &str, value: &str) -> Result<(), RepoError> {
        let broken = lock(&self.broken).clone();
        if let Some(e) = broken {
            return Err(e);
        }
        lock(&self.values).insert(name.to_owned(), value.to_owned());
        Ok(())
    }
}

// --------------------------------------------------------------------- session

/// In-memory [`SessionStore`].
#[derive(Debug, Default)]
pub struct InMemorySession {
    session: Mutex<Option<BrowserSession>>,
    broken: Mutex<Option<RepoError>>,
}

impl InMemorySession {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    pub fn break_with(&self, error: Option<RepoError>) {
        *lock(&self.broken) = error;
    }

    fn check(&self) -> Result<(), RepoError> {
        lock(&self.broken).clone().map_or(Ok(()), Err)
    }
}

#[async_trait]
impl SessionStore for InMemorySession {
    async fn load(&self) -> Result<Option<BrowserSession>, RepoError> {
        self.check()?;
        Ok(lock(&self.session).clone())
    }

    async fn save(&self, session: &BrowserSession) -> Result<(), RepoError> {
        self.check()?;
        *lock(&self.session) = Some(session.clone());
        Ok(())
    }

    async fn refresh(
        &self,
        session: &BrowserSession,
        based_on: Timestamp,
    ) -> Result<bool, RepoError> {
        self.check()?;
        let mut stored = lock(&self.session);
        if stored.as_ref().is_some_and(|s| s.updated_at == based_on) {
            *stored = Some(session.clone());
            return Ok(true);
        }
        Ok(false)
    }

    async fn clear(&self) -> Result<bool, RepoError> {
        self.check()?;
        Ok(lock(&self.session).take().is_some())
    }
}

// --------------------------------------------------------------------- library

#[derive(Debug, Default)]
struct LibraryState {
    files: HashMap<String, PathBuf>,
    publish_log: Vec<(String, PathBuf)>,
    fail_publish: bool,
}

/// A library that records what was published without touching the disk.
#[derive(Debug, Default)]
pub struct InMemoryLibrary {
    state: Mutex<LibraryState>,
}

impl InMemoryLibrary {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Pretend `file_name` is already in the library.
    pub fn seed(&self, file_name: &str) {
        lock(&self.state)
            .files
            .insert(file_name.to_owned(), PathBuf::from("seed"));
    }

    /// Simulate the user deleting a book from the folder behind our back.
    pub fn forget(&self, file_name: &str) {
        lock(&self.state).files.remove(file_name);
    }

    pub fn fail_publish(&self, fail: bool) {
        lock(&self.state).fail_publish = fail;
    }

    /// Every `(file_name, source)` ever published, in order.
    #[must_use]
    pub fn published(&self) -> Vec<(String, PathBuf)> {
        lock(&self.state).publish_log.clone()
    }
}

#[async_trait]
impl LibraryStore for InMemoryLibrary {
    fn path_of(&self, file_name: &str) -> PathBuf {
        PathBuf::from("/library").join(file_name)
    }

    async fn exists(&self, file_name: &str) -> Result<bool, StoreError> {
        Ok(lock(&self.state).files.contains_key(file_name))
    }

    async fn publish(&self, source: &Path, file_name: &str) -> Result<(), StoreError> {
        let mut state = lock(&self.state);
        if state.fail_publish {
            return Err(StoreError::Io("disk full".into()));
        }
        state
            .files
            .insert(file_name.to_owned(), source.to_path_buf());
        state
            .publish_log
            .push((file_name.to_owned(), source.to_path_buf()));
        Ok(())
    }
}

// ------------------------------------------------------------------ downloader

#[derive(Debug, Default)]
struct DownloaderState {
    script: VecDeque<Result<DownloadedBook, DownloadError>>,
    requests: Vec<DownloadRequest>,
    released: Vec<PathBuf>,
}

/// A downloader that plays back a script of results.
#[derive(Debug, Default)]
pub struct ScriptedDownloader {
    state: Mutex<DownloaderState>,
}

impl ScriptedDownloader {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    pub fn push_ok(&self, title: &str) {
        lock(&self.state).script.push_back(Ok(DownloadedBook {
            path: PathBuf::from(format!("/scratch/{title}.epub")),
            title: title.to_owned(),
            changed: true,
            log: String::new(),
        }));
    }

    /// An update that found nothing new.
    pub fn push_unchanged(&self, title: &str) {
        lock(&self.state).script.push_back(Ok(DownloadedBook {
            path: PathBuf::from(format!("/library/{title}.epub")),
            title: title.to_owned(),
            changed: false,
            log: "already contains all chapters".to_owned(),
        }));
    }

    pub fn push_err(&self, kind: FailureKind, message: &str) {
        lock(&self.state).script.push_back(Err(DownloadError {
            kind,
            message: message.to_owned(),
            log: format!("worker output for: {message}"),
        }));
    }

    #[must_use]
    pub fn requests(&self) -> Vec<DownloadRequest> {
        lock(&self.state).requests.clone()
    }

    /// Paths of every book handed back through `release`, in order.
    #[must_use]
    pub fn released(&self) -> Vec<PathBuf> {
        lock(&self.state).released.clone()
    }
}

#[async_trait]
impl Downloader for ScriptedDownloader {
    async fn download(&self, request: &DownloadRequest) -> Result<DownloadedBook, DownloadError> {
        let mut state = lock(&self.state);
        state.requests.push(request.clone());
        state.script.pop_front().unwrap_or_else(|| {
            Err(DownloadError {
                kind: FailureKind::Permanent,
                message: "script exhausted".into(),
                log: String::new(),
            })
        })
    }

    async fn release(&self, book: &DownloadedBook) {
        lock(&self.state).released.push(book.path.clone());
    }
}

// -------------------------------------------------------------------- notifier

#[derive(Debug, Default)]
pub struct RecordingNotifier {
    events: Mutex<Vec<NotifyEvent>>,
    failing: Mutex<bool>,
}

impl RecordingNotifier {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    pub fn fail(&self, failing: bool) {
        *lock(&self.failing) = failing;
    }

    #[must_use]
    pub fn events(&self) -> Vec<NotifyEvent> {
        lock(&self.events).clone()
    }
}

#[async_trait]
impl Notifier for RecordingNotifier {
    async fn notify(&self, event: &NotifyEvent) -> Result<(), NotifyError> {
        if *lock(&self.failing) {
            return Err(NotifyError("webhook down".into()));
        }
        lock(&self.events).push(event.clone());
        Ok(())
    }
}

// ------------------------------------------------------------------------ mail

#[derive(Debug, Default)]
struct MailState {
    pending: VecDeque<MailBatch>,
    committed: Vec<Checkpoint>,
    broken: bool,
}

/// Mail with real at-least-once semantics: a batch is re-delivered until its
/// checkpoint is committed.
#[derive(Debug, Default)]
pub struct InMemoryMail {
    state: Mutex<MailState>,
}

impl InMemoryMail {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    pub fn push(&self, batch: MailBatch) {
        lock(&self.state).pending.push_back(batch);
    }

    pub fn break_fetch(&self, broken: bool) {
        lock(&self.state).broken = broken;
    }

    #[must_use]
    pub fn committed(&self) -> Vec<Checkpoint> {
        lock(&self.state).committed.clone()
    }

    #[must_use]
    pub fn pending_batches(&self) -> usize {
        lock(&self.state).pending.len()
    }
}

#[async_trait]
impl MailSource for InMemoryMail {
    async fn fetch_new(&self) -> Result<MailBatch, MailError> {
        let state = lock(&self.state);
        if state.broken {
            return Err(MailError::Backend("connection refused".into()));
        }
        Ok(state.pending.front().cloned().unwrap_or_else(|| MailBatch {
            messages: Vec::new(),
            checkpoint: Checkpoint("idle".into()),
        }))
    }

    async fn commit(&self, checkpoint: &Checkpoint) -> Result<(), MailError> {
        let mut state = lock(&self.state);
        if state
            .pending
            .front()
            .is_some_and(|b| &b.checkpoint == checkpoint)
        {
            state.pending.pop_front();
        }
        state.committed.push(checkpoint.clone());
        Ok(())
    }
}
