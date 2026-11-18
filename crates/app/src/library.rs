//! Use cases behind the delete buttons: remove a book, or a series and its books.
//!
//! A book's metadata history is kept: only the file and the library's record of it go.
//! Deleting a series stops following it and deletes each of its books, except those that
//! also belong to another followed series (their files stay where they are, so a reader
//! keeps its place). Queued downloads of whatever is deleted are cancelled; while one is
//! running, nothing is deleted.

use std::sync::Arc;

use fanfold_domain::{
    job::{Job, JobId, JobState, Target},
    work::{SeriesId, WorkId},
};
use thiserror::Error;

use crate::{
    error::{RepoError, StoreError},
    ports::{
        BookCatalog, Clock, JobFilter, JobRepository, LibraryStore, LogEntry, LogLevel,
        MetadataStore,
    },
};

/// Largest number of active jobs looked at when checking what is queued or running.
const MAX_ACTIVE: u32 = 10_000;

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum DeleteError {
    #[error("{0} is downloading right now; try again when it finishes")]
    Busy(Target),
    #[error(transparent)]
    Repo(#[from] RepoError),
    #[error("cannot delete the file: {0}")]
    Store(#[from] StoreError),
}

/// What deleting a book did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeletedBook {
    pub work: WorkId,
    /// The library file that was deleted, if there was one.
    pub file: Option<String>,
    /// Queued downloads that were cancelled.
    pub cancelled: usize,
}

/// What deleting a series did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeletedSeries {
    pub series: SeriesId,
    /// Books deleted, by work.
    pub deleted: Vec<DeletedBook>,
    /// Works kept because they belong to another followed series (the first one named).
    pub kept: Vec<(WorkId, SeriesId)>,
    pub was_followed: bool,
    /// Queued downloads that were cancelled (the series' own and its works').
    pub cancelled: usize,
}

pub struct LibraryAdmin {
    jobs: Arc<dyn JobRepository>,
    catalog: Arc<dyn BookCatalog>,
    library: Arc<dyn LibraryStore>,
    metadata: Arc<dyn MetadataStore>,
    clock: Arc<dyn Clock>,
}

impl std::fmt::Debug for LibraryAdmin {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LibraryAdmin").finish_non_exhaustive()
    }
}

impl LibraryAdmin {
    #[must_use]
    pub const fn new(
        jobs: Arc<dyn JobRepository>,
        catalog: Arc<dyn BookCatalog>,
        library: Arc<dyn LibraryStore>,
        metadata: Arc<dyn MetadataStore>,
        clock: Arc<dyn Clock>,
    ) -> Self {
        Self {
            jobs,
            catalog,
            library,
            metadata,
            clock,
        }
    }

    /// Delete a work's book (and cancel its queued downloads). `log_to` is the job whose
    /// log records it.
    pub async fn delete_book(
        &self,
        work: WorkId,
        log_to: Option<JobId>,
    ) -> Result<DeletedBook, DeleteError> {
        let active = self.active_jobs().await?;
        refuse_if_running(&active, &[Target::Work(work)])?;
        let deleted = self.remove_book(work, &active).await?;
        if let Some(job) = log_to {
            let line = match &deleted.file {
                Some(file) => format!("deleted the book: {file}"),
                None => "there was no book to delete".to_owned(),
            };
            self.log(job, line).await;
        }
        Ok(deleted)
    }

    /// Stop following a series and delete its books, keeping those that belong to another
    /// followed series.
    pub async fn delete_series(
        &self,
        series: SeriesId,
        log_to: Option<JobId>,
    ) -> Result<DeletedSeries, DeleteError> {
        let members = self.metadata.series_members(series).await?;
        let mut doomed = Vec::new();
        let mut kept = Vec::new();
        for work in members {
            match self.other_followed_series(work, series).await? {
                Some(other) => kept.push((work, other)),
                None => doomed.push(work),
            }
        }
        let active = self.active_jobs().await?;
        let mut targets: Vec<Target> = doomed.iter().copied().map(Target::Work).collect();
        targets.push(Target::Series(series));
        refuse_if_running(&active, &targets)?;

        let mut cancelled = self.cancel_queued(&active, Target::Series(series)).await?;
        let was_followed = self.metadata.unfollow_series(series).await?;
        let mut deleted = Vec::new();
        for work in doomed {
            let book = self.remove_book(work, &active).await?;
            cancelled += book.cancelled;
            deleted.push(book);
        }
        if let Some(job) = log_to {
            let files = deleted.iter().filter(|d| d.file.is_some()).count();
            self.log(
                job,
                format!(
                    "deleted the series: {files} books deleted, {} kept for another series",
                    kept.len()
                ),
            )
            .await;
        }
        Ok(DeletedSeries {
            series,
            deleted,
            kept,
            was_followed,
            cancelled,
        })
    }

    async fn remove_book(&self, work: WorkId, active: &[Job]) -> Result<DeletedBook, DeleteError> {
        let cancelled = self.cancel_queued(active, Target::Work(work)).await?;
        let file = match self.catalog.get(work).await? {
            Some(record) => {
                // The file first: if that fails, the record still says where it is.
                self.library.remove(&record.file_name).await?;
                self.catalog.remove(work).await?;
                Some(record.file_name)
            }
            None => None,
        };
        Ok(DeletedBook {
            work,
            file,
            cancelled,
        })
    }

    /// The first other followed series the work belongs to, as its page last said.
    async fn other_followed_series(
        &self,
        work: WorkId,
        except: SeriesId,
    ) -> Result<Option<SeriesId>, RepoError> {
        let Some(state) = self.metadata.work_state(work).await? else {
            return Ok(None);
        };
        let others: Vec<SeriesId> = state
            .series
            .iter()
            .map(|p| p.id)
            .filter(|id| *id != except)
            .collect();
        Ok(self.metadata.followed(&others).await?.first().copied())
    }

    async fn active_jobs(&self) -> Result<Vec<Job>, RepoError> {
        self.jobs.list(JobFilter::Active, MAX_ACTIVE, 0).await
    }

    async fn cancel_queued(&self, active: &[Job], target: Target) -> Result<usize, RepoError> {
        let mut cancelled = 0;
        for job in active
            .iter()
            .filter(|j| j.target == target && j.state == JobState::Queued)
        {
            let mut job = job.clone();
            if job.cancel(self.clock.now()).is_ok() {
                self.jobs.save(&job).await?;
                cancelled += 1;
            }
        }
        Ok(cancelled)
    }

    async fn log(&self, job: JobId, message: String) {
        let entry = LogEntry {
            at: self.clock.now(),
            attempt: 0,
            level: LogLevel::Info,
            message,
        };
        if let Err(e) = self.jobs.append_log(job, &entry).await {
            tracing::warn!(%job, error = %e, "could not write job log");
        }
    }
}

fn refuse_if_running(active: &[Job], targets: &[Target]) -> Result<(), DeleteError> {
    match active
        .iter()
        .find(|j| j.state == JobState::Running && targets.contains(&j.target))
    {
        Some(job) => Err(DeleteError::Busy(job.target)),
        None => Ok(()),
    }
}
