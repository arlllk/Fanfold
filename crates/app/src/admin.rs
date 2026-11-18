//! Use cases behind the web UI's buttons: add a link (a work or a series), retry, cancel.

use std::sync::Arc;

use fanfold_domain::{
    job::{Job, JobId, JobSource, Target, TransitionError},
    work::{Ao3Link, UrlError, parse_ao3_link},
};
use thiserror::Error;

use crate::{
    error::RepoError,
    ports::{Clock, EnqueueOutcome, JobFilter, JobRepository},
};

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum AdminError {
    #[error(transparent)]
    InvalidUrl(#[from] UrlError),
    #[error("job {0} not found")]
    NotFound(JobId),
    #[error(transparent)]
    WrongState(#[from] TransitionError),
    #[error("a job for {0} is already queued or running")]
    AlreadyActive(Target),
    #[error(transparent)]
    Repo(RepoError),
}

/// Result of a bulk retry: nothing is all-or-nothing, so one stale selection does
/// not block the rest.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct BulkReport {
    pub retried: Vec<JobId>,
    pub skipped: Vec<(JobId, String)>,
}

/// Largest batch "retry all failed" will touch.
pub const MAX_BULK: u32 = 500;

pub struct JobAdmin {
    jobs: Arc<dyn JobRepository>,
    clock: Arc<dyn Clock>,
}

impl std::fmt::Debug for JobAdmin {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("JobAdmin").finish_non_exhaustive()
    }
}

impl JobAdmin {
    #[must_use]
    pub const fn new(jobs: Arc<dyn JobRepository>, clock: Arc<dyn Clock>) -> Self {
        Self { jobs, clock }
    }

    /// Queue an AO3 work, or a series (which lists it and queues its works), by URL.
    pub async fn add_url(&self, url: &str) -> Result<EnqueueOutcome, AdminError> {
        let target = match parse_ao3_link(url)? {
            Ao3Link::Work(work) => Target::Work(work),
            Ao3Link::Series(series) => Target::Series(series),
        };
        self.jobs
            .enqueue(target, JobSource::Manual, self.clock.now())
            .await
            .map_err(AdminError::Repo)
    }

    async fn load(&self, id: JobId) -> Result<Job, AdminError> {
        self.jobs
            .get(id)
            .await
            .map_err(AdminError::Repo)?
            .ok_or(AdminError::NotFound(id))
    }

    async fn store(&self, job: &Job) -> Result<(), AdminError> {
        self.jobs.save(job).await.map_err(|e| match e {
            RepoError::NotFound(id) => AdminError::NotFound(id),
            RepoError::Conflict(_) => AdminError::AlreadyActive(job.target),
            other @ RepoError::Backend(_) => AdminError::Repo(other),
        })
    }

    /// `Failed | Cancelled -> Queued` with a fresh retry budget.
    pub async fn retry(&self, id: JobId) -> Result<Job, AdminError> {
        let mut job = self.load(id).await?;
        job.retry(self.clock.now())?;
        self.store(&job).await?;
        Ok(job)
    }

    /// `Succeeded | Failed | Cancelled -> Queued`, downloading from scratch: the stored book
    /// is ignored (and replaced once the new download succeeds), not updated.
    pub async fn force_retry(&self, id: JobId) -> Result<Job, AdminError> {
        let mut job = self.load(id).await?;
        job.force_retry(self.clock.now())?;
        self.store(&job).await?;
        Ok(job)
    }

    /// `Queued -> Cancelled`.
    pub async fn cancel(&self, id: JobId) -> Result<Job, AdminError> {
        let mut job = self.load(id).await?;
        job.cancel(self.clock.now())?;
        self.store(&job).await?;
        Ok(job)
    }

    pub async fn retry_many(&self, ids: &[JobId]) -> BulkReport {
        let mut report = BulkReport::default();
        for id in ids {
            match self.retry(*id).await {
                Ok(_) => report.retried.push(*id),
                Err(e) => report.skipped.push((*id, e.to_string())),
            }
        }
        report
    }

    pub async fn retry_all_failed(&self) -> Result<BulkReport, AdminError> {
        let failed = self
            .jobs
            .list(JobFilter::Failed, MAX_BULK, 0)
            .await
            .map_err(AdminError::Repo)?;
        let ids: Vec<JobId> = failed.iter().map(|j| j.id).collect();
        Ok(self.retry_many(&ids).await)
    }
}
