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
        JobFilter, JobRepository, LibraryCatalog, LibraryEntry, LibraryPage, LibraryQuery,
        LibrarySort, LibraryStatus, LibraryStore, LibraryTotals, LogEntry, MailBatch, MailSource,
        MetadataStore, Notifier, NotifyEvent, Readiness, Recorded, SessionStore,
    },
};
use fanfold_domain::{
    Timestamp,
    failure::FailureKind,
    job::{Job, JobId, JobSource, JobState, Target},
    metadata::{
        Author, FollowSource, SeriesFacts, SeriesMetadata, SeriesPart, Tag, TagKind, WorkFacts,
        WorkMetadata, WorkState, WorkStats, plan_work_update,
    },
    session::BrowserSession,
    timestamp,
    work::{SeriesId, WorkId},
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
    works: HashMap<WorkId, MemWork>,
    series: HashMap<SeriesId, MemSeries>,
    next_version: u64,
    broken: Option<RepoError>,
    metadata_broken: Option<RepoError>,
}

/// One recorded version of a work.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkVersion {
    pub id: u64,
    pub seen_at: Timestamp,
    pub job: Option<JobId>,
    pub facts: WorkFacts,
}

/// One list item's lifetime, between the version that added it and the one that removed it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Span<T> {
    pub item: T,
    pub position: u32,
    pub added_in: u64,
    pub removed_in: Option<u64>,
}

/// Everything recorded about one work.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WorkHistory {
    pub first_seen: Option<Timestamp>,
    pub last_checked: Option<Timestamp>,
    pub versions: Vec<WorkVersion>,
    pub authors: Vec<Span<Author>>,
    pub tags: Vec<Span<Tag>>,
    pub series: Vec<Span<SeriesPart>>,
    pub chapters: Vec<Span<fanfold_domain::metadata::Chapter>>,
    pub stats: Vec<(Timestamp, WorkStats)>,
}

type MemWork = WorkHistory;

/// Everything recorded about one series.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SeriesHistory {
    pub followed: Option<(FollowSource, Timestamp)>,
    pub folder: Option<String>,
    pub versions: Vec<(Timestamp, SeriesFacts)>,
}

type MemSeries = SeriesHistory;

fn open_items<T: Clone>(spans: &[Span<T>]) -> Vec<T> {
    let mut open: Vec<&Span<T>> = spans.iter().filter(|s| s.removed_in.is_none()).collect();
    open.sort_by_key(|s| s.position);
    open.into_iter().map(|s| s.item.clone()).collect()
}

fn apply<T: Clone + PartialEq>(
    spans: &mut Vec<Span<T>>,
    changes: &fanfold_domain::metadata::SpanChanges<T>,
    version: u64,
) {
    for gone in &changes.closed {
        if let Some(span) = spans
            .iter_mut()
            .find(|s| s.removed_in.is_none() && &s.item == gone)
        {
            span.removed_in = Some(version);
        }
    }
    for (position, item) in &changes.opened {
        spans.push(Span {
            item: item.clone(),
            position: *position,
            added_in: version,
            removed_in: None,
        });
    }
}

impl WorkHistory {
    fn state(&self) -> Option<WorkState> {
        let latest = self.versions.last()?;
        Some(WorkState {
            facts: latest.facts.clone(),
            authors: open_items(&self.authors),
            tags: open_items(&self.tags),
            series: open_items(&self.series),
            chapters: open_items(&self.chapters),
        })
    }
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

    /// Make only the [`MetadataStore`] calls fail with `error`, or heal with `None`.
    pub fn break_metadata_with(&self, error: Option<RepoError>) {
        lock(&self.state).metadata_broken = error;
    }

    fn metadata_state(&self) -> Result<MutexGuard<'_, JobsState>, RepoError> {
        let state = self.state()?;
        match &state.metadata_broken {
            Some(e) => Err(e.clone()),
            None => Ok(state),
        }
    }

    /// Everything recorded about a work (empty if nothing was).
    #[must_use]
    pub fn work_history(&self, work: WorkId) -> WorkHistory {
        lock(&self.state)
            .works
            .get(&work)
            .cloned()
            .unwrap_or_default()
    }

    /// Everything recorded about a series (empty if nothing was).
    #[must_use]
    pub fn series_history(&self, series: SeriesId) -> SeriesHistory {
        lock(&self.state)
            .series
            .get(&series)
            .cloned()
            .unwrap_or_default()
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
        target: Target,
        source: JobSource,
        now: Timestamp,
    ) -> Result<EnqueueOutcome, RepoError> {
        let mut state = self.state()?;
        if let Some(active) = state
            .jobs
            .values()
            .find(|j| j.target == target && j.state.is_active())
        {
            return Ok(EnqueueOutcome::AlreadyActive(active.id));
        }
        state.next_id += 1;
        let job = Job::new(JobId::new(state.next_id), target, source, now);
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
                other.id != job.id && other.target == job.target && other.state.is_active()
            });
        if clashes {
            return Err(RepoError::Conflict(format!(
                "{} already has an active job",
                job.target
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

    async fn remove(&self, work: WorkId) -> Result<bool, RepoError> {
        Ok(self.state()?.books.remove(&work).is_some())
    }
}

/// The tags a library listing carries.
const fn listed_tag(kind: TagKind) -> bool {
    matches!(
        kind,
        TagKind::Fandom | TagKind::Relationship | TagKind::Warning
    )
}

fn library_entry(state: &JobsState, book: &BookRecord) -> LibraryEntry {
    let history = state.works.get(&book.work);
    LibraryEntry {
        book: book.clone(),
        facts: history
            .and_then(|h| h.versions.last())
            .map(|v| v.facts.clone()),
        tags: history.map_or_default(|h| {
            open_items(&h.tags)
                .into_iter()
                .filter(|t| listed_tag(t.kind))
                .collect()
        }),
        series: history.map_or_default(|h| open_items(&h.series)),
        // The latest observation; on equal times the one recorded last (as the database).
        stats: history
            .and_then(|h| {
                h.stats
                    .iter()
                    .enumerate()
                    .max_by_key(|(i, (at, _))| (*at, *i))
                    .map(|(_, (_, s))| *s)
            })
            .unwrap_or_default(),
        last_checked: history.and_then(|h| h.last_checked),
    }
}

/// Whether an entry matches a (lower-cased) search, as `LibraryQuery::search` defines it.
fn library_matches(state: &JobsState, entry: &LibraryEntry, needle: &str) -> bool {
    let has = |s: &str| s.to_lowercase().contains(needle);
    if has(entry.title()) || entry.facts.as_ref().is_some_and(|f| has(&f.byline)) {
        return true;
    }
    state.works.get(&entry.book.work).is_some_and(|h| {
        open_items(&h.tags).iter().any(|t| has(&t.name))
            || open_items(&h.series).iter().any(|s| has(&s.name))
    })
}

fn library_order(sort: LibrarySort, a: &LibraryEntry, b: &LibraryEntry) -> std::cmp::Ordering {
    use std::cmp::{Ordering, Reverse};
    let lower = |e: &LibraryEntry| e.title().to_lowercase();
    // `None` sorts last, ascending or descending, as `NULLS LAST` does.
    let byline = |e: &LibraryEntry| {
        e.facts
            .as_ref()
            .map(|f| f.byline.to_lowercase())
            .map_or((1, String::new()), |b| (0, b))
    };
    let work = |e: &LibraryEntry| e.book.work;
    let ordering: Ordering = match sort {
        LibrarySort::Updated => {
            let updated = |e: &LibraryEntry| e.facts.as_ref().and_then(|f| f.updated_on);
            (updated(a).is_none(), Reverse(updated(a)))
                .cmp(&(updated(b).is_none(), Reverse(updated(b))))
                .then_with(|| b.book.updated_at.cmp(&a.book.updated_at))
                .then_with(|| work(b).cmp(&work(a)))
        }
        LibrarySort::Title => lower(a).cmp(&lower(b)).then_with(|| work(a).cmp(&work(b))),
        LibrarySort::Author => byline(a)
            .cmp(&byline(b))
            .then_with(|| lower(a).cmp(&lower(b)))
            .then_with(|| work(a).cmp(&work(b))),
        LibrarySort::Words => {
            let words = |e: &LibraryEntry| e.facts.as_ref().map(|f| f.words);
            (words(a).is_none(), Reverse(words(a)))
                .cmp(&(words(b).is_none(), Reverse(words(b))))
                .then_with(|| work(a).cmp(&work(b)))
        }
    };
    ordering
}

#[async_trait]
impl LibraryCatalog for InMemoryJobs {
    async fn library(&self, query: &LibraryQuery) -> Result<LibraryPage, RepoError> {
        let state = self.metadata_state()?;
        let all: Vec<LibraryEntry> = state
            .books
            .values()
            .map(|b| library_entry(&state, b))
            .collect();
        let totals = LibraryTotals {
            books: all.len() as u64,
            words: all
                .iter()
                .filter_map(|e| e.facts.as_ref())
                .map(|f| f.words)
                .sum(),
            complete: all.iter().filter(|e| e.complete() == Some(true)).count() as u64,
        };
        let needle = query
            .search
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_lowercase);
        let mut matching: Vec<LibraryEntry> = all
            .into_iter()
            .filter(|e| match query.status {
                LibraryStatus::All => true,
                LibraryStatus::Complete => e.complete() == Some(true),
                LibraryStatus::InProgress => e.complete() == Some(false),
            })
            .filter(|e| {
                needle
                    .as_deref()
                    .is_none_or(|n| library_matches(&state, e, n))
            })
            .collect();
        matching.sort_by(|a, b| library_order(query.sort, a, b));
        let count = matching.len() as u64;
        let entries = matching
            .into_iter()
            .skip(query.offset as usize)
            .take(query.limit as usize)
            .collect();
        Ok(LibraryPage {
            entries,
            matching: count,
            totals,
        })
    }

    async fn entry(&self, work: WorkId) -> Result<Option<LibraryEntry>, RepoError> {
        let state = self.metadata_state()?;
        Ok(state.books.get(&work).map(|b| library_entry(&state, b)))
    }
}

#[async_trait]
impl MetadataStore for InMemoryJobs {
    async fn record_work(
        &self,
        metadata: &WorkMetadata,
        seen_at: Timestamp,
        job: Option<JobId>,
    ) -> Result<Recorded, RepoError> {
        let mut state = self.metadata_state()?;
        let next_version = state.next_version + 1;
        let work = state.works.entry(metadata.work).or_default();
        work.first_seen.get_or_insert(seen_at);
        work.last_checked = Some(seen_at);
        work.stats.push((seen_at, metadata.stats));
        let observed = metadata.state();
        let update = plan_work_update(work.state().as_ref(), &observed);
        if update.new_version {
            work.versions.push(WorkVersion {
                id: next_version,
                seen_at,
                job,
                facts: observed.facts,
            });
            apply(&mut work.authors, &update.authors, next_version);
            apply(&mut work.tags, &update.tags, next_version);
            apply(&mut work.series, &update.series, next_version);
            apply(&mut work.chapters, &update.chapters, next_version);
            state.next_version = next_version;
        }
        Ok(Recorded {
            new_version: update.new_version,
        })
    }

    async fn record_series(
        &self,
        metadata: &SeriesMetadata,
        seen_at: Timestamp,
        _job: Option<JobId>,
    ) -> Result<Recorded, RepoError> {
        let mut state = self.metadata_state()?;
        let series = state.series.entry(metadata.series).or_default();
        let changed = series.versions.last().map(|(_, f)| f) != Some(&metadata.facts);
        if changed {
            series.versions.push((seen_at, metadata.facts.clone()));
        }
        Ok(Recorded {
            new_version: changed,
        })
    }

    async fn work_state(&self, work: WorkId) -> Result<Option<WorkState>, RepoError> {
        Ok(self
            .metadata_state()?
            .works
            .get(&work)
            .and_then(WorkHistory::state))
    }

    async fn follow_series(
        &self,
        series: SeriesId,
        by: FollowSource,
        at: Timestamp,
    ) -> Result<(), RepoError> {
        let mut state = self.metadata_state()?;
        state
            .series
            .entry(series)
            .or_default()
            .followed
            .get_or_insert((by, at));
        Ok(())
    }

    async fn unfollow_series(&self, series: SeriesId) -> Result<bool, RepoError> {
        let mut state = self.metadata_state()?;
        Ok(state
            .series
            .get_mut(&series)
            .and_then(|s| s.followed.take())
            .is_some())
    }

    async fn series_members(&self, series: SeriesId) -> Result<Vec<WorkId>, RepoError> {
        let state = self.metadata_state()?;
        let mut members: Vec<WorkId> = state
            .works
            .iter()
            .filter(|(_, w)| {
                w.series
                    .iter()
                    .any(|s| s.removed_in.is_none() && s.item.id == series)
            })
            .map(|(id, _)| *id)
            .collect();
        if let Some((_, listing)) = state.series.get(&series).and_then(|s| s.versions.last()) {
            members.extend(listing.works.iter().copied());
        }
        members.sort();
        members.dedup();
        Ok(members)
    }

    async fn followed(&self, candidates: &[SeriesId]) -> Result<Vec<SeriesId>, RepoError> {
        let state = self.metadata_state()?;
        Ok(candidates
            .iter()
            .copied()
            .filter(|id| state.series.get(id).is_some_and(|s| s.followed.is_some()))
            .collect())
    }

    async fn claim_series_folder(
        &self,
        series: SeriesId,
        proposed: &str,
    ) -> Result<String, RepoError> {
        let mut state = self.metadata_state()?;
        Ok(state
            .series
            .entry(series)
            .or_default()
            .folder
            .get_or_insert_with(|| proposed.to_owned())
            .clone())
    }

    async fn series_names(
        &self,
        series: &[SeriesId],
    ) -> Result<HashMap<SeriesId, String>, RepoError> {
        let state = self.metadata_state()?;
        let from_works = |id: SeriesId| {
            state.works.values().find_map(|w| {
                w.series
                    .iter()
                    .rev()
                    .find(|s| s.item.id == id)
                    .map(|s| s.item.name.clone())
            })
        };
        Ok(series
            .iter()
            .filter_map(|id| {
                state
                    .series
                    .get(id)
                    .and_then(|s| s.versions.last().map(|(_, f)| f.name.clone()))
                    .or_else(|| from_works(*id))
                    .map(|name| (*id, name))
            })
            .collect())
    }
}

/// Plausible metadata for a work, as a download would report it: one author, one fandom,
/// no series. Tests adjust what they need.
#[must_use]
pub fn sample_metadata(work: WorkId, title: &str) -> WorkMetadata {
    WorkMetadata {
        work,
        facts: WorkFacts {
            title: title.to_owned(),
            byline: "Author".to_owned(),
            summary_html: String::new(),
            language: "English".to_owned(),
            rating: "General Audiences".to_owned(),
            restricted: false,
            status: "In-Progress".to_owned(),
            published_on: None,
            updated_on: None,
            chapters_posted: 1,
            chapters_total: None,
            words: 1000,
        },
        authors: vec![Author {
            user: "Author".to_owned(),
            pseud: "Author".to_owned(),
        }],
        tags: vec![Tag {
            kind: fanfold_domain::metadata::TagKind::Fandom,
            name: "Some Fandom".to_owned(),
        }],
        series: vec![],
        chapters: vec![],
        stats: WorkStats::default(),
        raw: serde_json::Value::Null,
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
    removed: Vec<String>,
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

    /// Every file `remove` was asked for, in order.
    #[must_use]
    pub fn removed(&self) -> Vec<String> {
        lock(&self.state).removed.clone()
    }

    /// The files in the library now.
    #[must_use]
    pub fn files(&self) -> Vec<String> {
        let mut files: Vec<String> = lock(&self.state).files.keys().cloned().collect();
        files.sort();
        files
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

    async fn remove(&self, file_name: &str) -> Result<bool, StoreError> {
        let mut state = lock(&self.state);
        state.removed.push(file_name.to_owned());
        Ok(state.files.remove(file_name).is_some())
    }
}

// ------------------------------------------------------------------ downloader

#[derive(Debug, Default)]
struct DownloaderState {
    script: VecDeque<Result<DownloadedBook, DownloadError>>,
    listings: VecDeque<Result<SeriesMetadata, DownloadError>>,
    requests: Vec<DownloadRequest>,
    listed: Vec<SeriesId>,
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

    /// A successful download; its metadata is [`sample_metadata`] for the requested work.
    pub fn push_ok(&self, title: &str) {
        self.push_ok_with(title, |_| {});
    }

    /// A successful download whose [`sample_metadata`] `adjust` changes first.
    pub fn push_ok_with(&self, title: &str, adjust: impl FnOnce(&mut WorkMetadata)) {
        // The real work id is filled in when the download is requested.
        let Some(placeholder) = WorkId::new(1) else {
            return;
        };
        let mut metadata = sample_metadata(placeholder, title);
        adjust(&mut metadata);
        lock(&self.state).script.push_back(Ok(DownloadedBook {
            path: PathBuf::from(format!("/scratch/{title}.epub")),
            title: title.to_owned(),
            changed: true,
            log: String::new(),
            metadata: Some(metadata),
        }));
    }

    /// The next `list_series` answer: a series with these works.
    pub fn push_listing(&self, name: &str, works: &[u64]) {
        // The real series id is filled in when the listing is requested.
        let Some(placeholder) = SeriesId::new(1) else {
            return;
        };
        lock(&self.state).listings.push_back(Ok(SeriesMetadata {
            series: placeholder,
            facts: SeriesFacts {
                name: name.to_owned(),
                summary_html: String::new(),
                complete: Some(false),
                works: works.iter().filter_map(|w| WorkId::new(*w)).collect(),
            },
            raw: serde_json::Value::Null,
        }));
    }

    /// The next `list_series` answer: a failure.
    pub fn push_listing_err(&self, kind: FailureKind, message: &str) {
        lock(&self.state).listings.push_back(Err(DownloadError {
            kind,
            message: message.to_owned(),
            log: String::new(),
        }));
    }

    /// Every series `list_series` was asked for, in order.
    #[must_use]
    pub fn listed(&self) -> Vec<SeriesId> {
        lock(&self.state).listed.clone()
    }

    /// An update that found nothing new.
    pub fn push_unchanged(&self, title: &str) {
        lock(&self.state).script.push_back(Ok(DownloadedBook {
            path: PathBuf::from(format!("/library/{title}.epub")),
            title: title.to_owned(),
            changed: false,
            log: "already contains all chapters".to_owned(),
            metadata: None,
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
        state
            .script
            .pop_front()
            .unwrap_or_else(|| {
                Err(DownloadError {
                    kind: FailureKind::Permanent,
                    message: "script exhausted".into(),
                    log: String::new(),
                })
            })
            .map(|mut book| {
                if let Some(metadata) = &mut book.metadata {
                    metadata.work = request.work;
                }
                book
            })
    }

    async fn list_series(&self, series: SeriesId) -> Result<SeriesMetadata, DownloadError> {
        let mut state = lock(&self.state);
        state.listed.push(series);
        state
            .listings
            .pop_front()
            .unwrap_or_else(|| {
                Err(DownloadError {
                    kind: FailureKind::Permanent,
                    message: "listing script exhausted".into(),
                    log: String::new(),
                })
            })
            .map(|mut listing| {
                listing.series = series;
                listing
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
