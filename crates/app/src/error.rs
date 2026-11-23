//! Error types. Each port has its own, so adapters report in the application's
//! vocabulary and never leak driver-specific types upward.

use fanfold_domain::job::{JobId, TransitionError};
use thiserror::Error;

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum RepoError {
    #[error("job {0} not found")]
    NotFound(JobId),
    /// The change would break an invariant, e.g. a second active job for one work.
    #[error("conflict: {0}")]
    Conflict(String),
    #[error("storage backend failure: {0}")]
    Backend(String),
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum MailError {
    #[error("mail backend failure: {0}")]
    Backend(String),
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum StoreError {
    #[error("invalid book file name: {0:?}")]
    InvalidName(String),
    #[error("library failure: {0}")]
    Io(String),
}

/// The search index could not be built or read: a bug, or the machine is out of memory.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[error("search index failure: {0}")]
pub struct SearchError(pub String);

#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[error("notification failed: {0}")]
pub struct NotifyError(pub String);

/// What a use case can fail with. A [`AppError::Invariant`] means a bug: the
/// code asked a job for a transition its state does not allow.
#[derive(Debug, Error)]
pub enum AppError {
    #[error(transparent)]
    Repo(#[from] RepoError),
    #[error(transparent)]
    Mail(#[from] MailError),
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error("invariant violated: {0}")]
    Invariant(#[from] TransitionError),
}
