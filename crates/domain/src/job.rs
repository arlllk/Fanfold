//! The download job and its state machine.
//!
//! ```text
//!            start                 succeed
//!   Queued ─────────► Running ───────────────► Succeeded
//!     ▲  ▲               │ fail (retryable)
//!     │  └───────────────┤
//!     │ retry            │ fail (permanent / budget exhausted)
//!     │                  ▼
//!     └──────────────  Failed         Queued ──cancel──► Cancelled
//! ```
//!
//! "Retrying" is not a state: it is `Queued` with `attempt > 0`.
//!
//! A *forced* retry (`force_retry`, also from `Succeeded`) re-queues with `force` set: the
//! next download ignores the stored book and starts from scratch, until one succeeds.

use std::{fmt, str::FromStr};

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::{
    Timestamp,
    failure::FailureKind,
    retry::RetryPolicy,
    timestamp,
    work::{SeriesId, WorkId},
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct JobId(i64);

impl JobId {
    #[must_use]
    pub const fn new(id: i64) -> Self {
        Self(id)
    }

    #[must_use]
    pub const fn get(self) -> i64 {
        self.0
    }
}

impl fmt::Display for JobId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

macro_rules! string_enum {
    ($(#[$meta:meta])* $name:ident, $err:ident, { $($variant:ident => $text:literal),+ $(,)? }) => {
        $(#[$meta])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
        #[serde(rename_all = "snake_case")]
        pub enum $name { $($variant),+ }

        impl $name {
            pub const ALL: &'static [Self] = &[$(Self::$variant),+];

            #[must_use]
            pub const fn as_str(self) -> &'static str {
                match self { $(Self::$variant => $text),+ }
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(self.as_str())
            }
        }

        #[derive(Debug, Clone, PartialEq, Eq, Error)]
        #[error("unknown {what}: {0}", what = stringify!($name))]
        pub struct $err(pub String);

        impl FromStr for $name {
            type Err = $err;

            fn from_str(s: &str) -> Result<Self, Self::Err> {
                Self::ALL.iter().copied().find(|v| v.as_str() == s).ok_or_else(|| $err(s.to_owned()))
            }
        }
    };
}

string_enum!(
    /// Where a job came from.
    JobSource, UnknownJobSource, { Mail => "mail", Manual => "manual" }
);

string_enum!(
    JobState, UnknownJobState, {
        Queued => "queued",
        Running => "running",
        Succeeded => "succeeded",
        Failed => "failed",
        Cancelled => "cancelled",
    }
);

string_enum!(
    /// What kind of thing a job fetches.
    TargetKind, UnknownTargetKind, { Work => "work", Series => "series" }
);

/// What a job fetches: one work (download it), or a series (list it and queue its works).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", content = "id", rename_all = "snake_case")]
pub enum Target {
    Work(WorkId),
    Series(SeriesId),
}

impl Target {
    #[must_use]
    pub const fn kind(self) -> TargetKind {
        match self {
            Self::Work(_) => TargetKind::Work,
            Self::Series(_) => TargetKind::Series,
        }
    }

    /// The AO3 id, in storage form.
    #[must_use]
    pub fn id_i64(self) -> i64 {
        match self {
            Self::Work(w) => w.to_i64(),
            Self::Series(s) => s.to_i64(),
        }
    }

    /// Inverse of `(kind(), id_i64())`.
    #[must_use]
    pub fn from_parts(kind: TargetKind, id: i64) -> Option<Self> {
        match kind {
            TargetKind::Work => WorkId::from_i64(id).map(Self::Work),
            TargetKind::Series => SeriesId::from_i64(id).map(Self::Series),
        }
    }

    #[must_use]
    pub const fn work(self) -> Option<WorkId> {
        match self {
            Self::Work(w) => Some(w),
            Self::Series(_) => None,
        }
    }

    #[must_use]
    pub fn canonical_url(self) -> String {
        match self {
            Self::Work(w) => w.canonical_url(),
            Self::Series(s) => s.canonical_url(),
        }
    }
}

impl From<WorkId> for Target {
    fn from(work: WorkId) -> Self {
        Self::Work(work)
    }
}

impl From<SeriesId> for Target {
    fn from(series: SeriesId) -> Self {
        Self::Series(series)
    }
}

impl fmt::Display for Target {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Work(w) => write!(f, "work {w}"),
            Self::Series(s) => write!(f, "series {s}"),
        }
    }
}

impl JobState {
    /// Queued or running: at most one such job may exist per work.
    #[must_use]
    pub const fn is_active(self) -> bool {
        matches!(self, Self::Queued | Self::Running)
    }
}

/// An action was attempted from a state that does not allow it.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[error("cannot {action} a job that is {from}")]
pub struct TransitionError {
    pub from: JobState,
    pub action: &'static str,
}

/// What happened to a job that failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Resolution {
    Retrying { at: Timestamp },
    GaveUp,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Job {
    pub id: JobId,
    pub target: Target,
    pub source: JobSource,
    pub state: JobState,
    /// Failures that consumed retry budget so far.
    pub attempt: u32,
    /// Earliest time a queued job may be claimed.
    pub next_attempt_at: Timestamp,
    pub created_at: Timestamp,
    pub updated_at: Timestamp,
    pub last_error: Option<String>,
    /// Download from scratch, ignoring the stored book (a forced retry). Cleared on success.
    #[serde(default)]
    pub force: bool,
}

impl Job {
    #[must_use]
    pub fn new(id: JobId, target: impl Into<Target>, source: JobSource, now: Timestamp) -> Self {
        Self {
            id,
            target: target.into(),
            source,
            state: JobState::Queued,
            attempt: 0,
            next_attempt_at: now,
            created_at: now,
            updated_at: now,
            last_error: None,
            force: false,
        }
    }

    fn expect(&self, allowed: &[JobState], action: &'static str) -> Result<(), TransitionError> {
        if allowed.contains(&self.state) {
            Ok(())
        } else {
            Err(TransitionError {
                from: self.state,
                action,
            })
        }
    }

    /// `Queued -> Running`.
    pub fn start(&mut self, now: Timestamp) -> Result<(), TransitionError> {
        self.expect(&[JobState::Queued], "start")?;
        self.state = JobState::Running;
        self.updated_at = now;
        Ok(())
    }

    /// `Running -> Succeeded`.
    pub fn succeed(&mut self, now: Timestamp) -> Result<(), TransitionError> {
        self.expect(&[JobState::Running], "succeed")?;
        self.state = JobState::Succeeded;
        self.updated_at = now;
        self.last_error = None;
        self.force = false;
        Ok(())
    }

    /// `Running -> Queued` (retry later) or `Running -> Failed`.
    pub fn fail(
        &mut self,
        now: Timestamp,
        kind: FailureKind,
        message: String,
        policy: &RetryPolicy,
        jitter: f64,
    ) -> Result<Resolution, TransitionError> {
        self.expect(&[JobState::Running], "fail")?;
        self.updated_at = now;
        self.last_error = Some(message);
        if kind.consumes_attempt() {
            self.attempt = self.attempt.saturating_add(1);
        }
        let exhausted = kind.consumes_attempt() && self.attempt >= policy.max_attempts;
        if !kind.is_retryable() || exhausted {
            self.state = JobState::Failed;
            return Ok(Resolution::GaveUp);
        }
        let delay = policy.delay(self.attempt.saturating_sub(1), jitter);
        let at = timestamp::add(now, delay);
        self.state = JobState::Queued;
        self.next_attempt_at = at;
        Ok(Resolution::Retrying { at })
    }

    /// `Failed | Cancelled -> Queued`, with a fresh retry budget.
    pub fn retry(&mut self, now: Timestamp) -> Result<(), TransitionError> {
        self.expect(&[JobState::Failed, JobState::Cancelled], "retry")?;
        self.state = JobState::Queued;
        self.attempt = 0;
        self.next_attempt_at = now;
        self.updated_at = now;
        Ok(())
    }

    /// `Succeeded | Failed | Cancelled -> Queued`, with a fresh retry budget and `force` set:
    /// for a book that was downloaded wrong, which a normal update would only extend.
    pub fn force_retry(&mut self, now: Timestamp) -> Result<(), TransitionError> {
        self.expect(
            &[JobState::Succeeded, JobState::Failed, JobState::Cancelled],
            "force a retry of",
        )?;
        self.state = JobState::Queued;
        self.attempt = 0;
        self.next_attempt_at = now;
        self.updated_at = now;
        self.force = true;
        Ok(())
    }

    /// `Queued -> Cancelled`.
    pub fn cancel(&mut self, now: Timestamp) -> Result<(), TransitionError> {
        self.expect(&[JobState::Queued], "cancel")?;
        self.state = JobState::Cancelled;
        self.updated_at = now;
        Ok(())
    }

    /// `Running -> Queued` without spending budget: used after a crash, when a
    /// job was left running by a process that no longer exists.
    pub fn requeue_orphan(&mut self, now: Timestamp) -> Result<(), TransitionError> {
        self.expect(&[JobState::Running], "requeue")?;
        self.state = JobState::Queued;
        self.next_attempt_at = now;
        self.updated_at = now;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use proptest::prelude::*;

    use super::*;

    const POLICY: RetryPolicy = RetryPolicy {
        max_attempts: 3,
        base: Duration::from_secs(60),
        cap: Duration::from_secs(3600),
    };

    fn t(secs: i64) -> Timestamp {
        Timestamp::from_second(secs).unwrap()
    }

    fn running() -> Job {
        let mut job = Job::new(
            JobId::new(1),
            WorkId::new(7).unwrap(),
            JobSource::Manual,
            t(0),
        );
        job.start(t(1)).unwrap();
        job
    }

    #[test]
    fn job_ids_expose_their_value() {
        assert_eq!(JobId::new(7).get(), 7);
        assert_eq!(JobId::new(-3).get(), -3);
        assert_eq!(JobId::new(7).to_string(), "7");
    }

    #[test]
    fn happy_path() {
        let mut job = running();
        job.succeed(t(2)).unwrap();
        assert_eq!(job.state, JobState::Succeeded);
        assert_eq!(job.updated_at, t(2));
        assert!(job.last_error.is_none());
    }

    #[test]
    fn transient_failure_requeues_with_backoff() {
        let mut job = running();
        let res = job
            .fail(t(10), FailureKind::Transient, "boom".into(), &POLICY, 0.5)
            .unwrap();
        assert_eq!(res, Resolution::Retrying { at: t(70) });
        assert_eq!(job.state, JobState::Queued);
        assert_eq!(job.attempt, 1);
        assert_eq!(job.next_attempt_at, t(70));
        assert_eq!(job.last_error.as_deref(), Some("boom"));
    }

    #[test]
    fn budget_exhaustion_gives_up() {
        let mut job = running();
        job.attempt = POLICY.max_attempts - 1;
        let res = job
            .fail(t(10), FailureKind::Transient, "x".into(), &POLICY, 0.5)
            .unwrap();
        assert_eq!(res, Resolution::GaveUp);
        assert_eq!(job.state, JobState::Failed);
    }

    #[test]
    fn permanent_failure_gives_up_immediately() {
        for kind in [
            FailureKind::Permanent,
            FailureKind::NotFound,
            FailureKind::AuthFailed,
        ] {
            let mut job = running();
            let res = job.fail(t(10), kind, "x".into(), &POLICY, 0.5).unwrap();
            assert_eq!(res, Resolution::GaveUp, "{kind}");
        }
    }

    #[test]
    fn targets_roundtrip_through_storage_form_and_serde() {
        let work = Target::Work(WorkId::new(5).unwrap());
        let series = Target::Series(SeriesId::new(5).unwrap());
        assert_ne!(work, series, "same number, different things");
        for t in [work, series] {
            assert_eq!(Target::from_parts(t.kind(), t.id_i64()), Some(t));
            let json = serde_json::to_string(&t).unwrap();
            assert_eq!(serde_json::from_str::<Target>(&json).unwrap(), t);
        }
        assert_eq!(series.to_string(), "series 5");
        assert_eq!(
            series.canonical_url(),
            "https://archiveofourown.org/series/5"
        );
        assert_eq!(Target::from_parts(TargetKind::Work, 0), None);
        assert_eq!("series".parse::<TargetKind>(), Ok(TargetKind::Series));
    }

    #[test]
    fn site_outage_never_spends_budget() {
        let mut job = running();
        for round in 0..50 {
            let res = job
                .fail(
                    t(10 + round),
                    FailureKind::SiteUnavailable,
                    "503".into(),
                    &POLICY,
                    0.5,
                )
                .unwrap();
            assert!(matches!(res, Resolution::Retrying { .. }));
            assert_eq!(job.attempt, 0);
            job.start(t(11 + round)).unwrap();
        }
    }

    #[test]
    fn blocked_spends_budget() {
        let mut job = running();
        job.fail(t(10), FailureKind::Blocked, "403".into(), &POLICY, 0.5)
            .unwrap();
        assert_eq!(job.attempt, 1);
    }

    #[test]
    fn manual_retry_resets_budget() {
        let mut job = running();
        job.attempt = 2;
        job.fail(t(10), FailureKind::Permanent, "x".into(), &POLICY, 0.5)
            .unwrap();
        job.retry(t(20)).unwrap();
        assert_eq!(
            (job.state, job.attempt, job.next_attempt_at),
            (JobState::Queued, 0, t(20))
        );
    }

    #[test]
    fn a_forced_retry_requeues_even_a_finished_job_until_a_download_succeeds() {
        let mut job = running();
        job.succeed(t(10)).unwrap();
        job.force_retry(t(20)).unwrap();
        assert_eq!(
            (job.state, job.attempt, job.next_attempt_at, job.force),
            (JobState::Queued, 0, t(20), true)
        );
        // A failed forced run stays forced on its retries...
        job.start(t(21)).unwrap();
        job.fail(
            t(22),
            FailureKind::SiteUnavailable,
            "525".into(),
            &POLICY,
            0.5,
        )
        .unwrap();
        assert!(job.force && job.state == JobState::Queued);
        // ...and a successful one ends it.
        job.start(t(30)).unwrap();
        job.succeed(t(31)).unwrap();
        assert!(!job.force);

        let mut failed = running();
        failed.attempt = 4;
        failed
            .fail(t(10), FailureKind::Permanent, "x".into(), &POLICY, 0.5)
            .unwrap();
        failed.force_retry(t(11)).unwrap();
        assert_eq!((failed.state, failed.attempt), (JobState::Queued, 0));
    }

    #[test]
    fn a_forced_retry_of_an_active_job_is_rejected() {
        let mut queued = Job::new(
            JobId::new(1),
            WorkId::new(7).unwrap(),
            JobSource::Mail,
            t(0),
        );
        assert!(queued.force_retry(t(1)).is_err());
        assert!(!queued.force);
        queued.start(t(1)).unwrap();
        assert!(queued.force_retry(t(2)).is_err());
        assert!(!queued.force);
    }

    #[test]
    fn orphan_requeue_keeps_budget() {
        let mut job = running();
        job.attempt = 2;
        job.requeue_orphan(t(5)).unwrap();
        assert_eq!((job.state, job.attempt), (JobState::Queued, 2));
    }

    #[test]
    fn illegal_transitions_are_rejected() {
        let queued = Job::new(
            JobId::new(1),
            WorkId::new(7).unwrap(),
            JobSource::Mail,
            t(0),
        );
        let mut j = queued;
        assert!(j.succeed(t(1)).is_err());
        assert!(
            j.fail(t(1), FailureKind::Transient, String::new(), &POLICY, 0.0)
                .is_err()
        );
        assert!(j.retry(t(1)).is_err());
        assert!(j.requeue_orphan(t(1)).is_err());
        j.start(t(1)).unwrap();
        assert!(j.start(t(2)).is_err());
        assert!(j.cancel(t(2)).is_err());
        let mut done = running();
        done.succeed(t(2)).unwrap();
        for result in [
            done.start(t(3)),
            done.cancel(t(3)),
            done.retry(t(3)),
            done.succeed(t(3)),
        ] {
            assert!(result.is_err());
        }
        // A rejected transition changes nothing.
        assert_eq!(done.state, JobState::Succeeded);
        assert_eq!(done.updated_at, t(2));
    }

    #[test]
    fn cancel_then_retry() {
        let mut job = Job::new(
            JobId::new(1),
            WorkId::new(7).unwrap(),
            JobSource::Manual,
            t(0),
        );
        job.cancel(t(1)).unwrap();
        assert_eq!(job.state, JobState::Cancelled);
        job.retry(t(2)).unwrap();
        assert_eq!(job.state, JobState::Queued);
    }

    #[test]
    fn enum_strings_roundtrip() {
        for s in JobState::ALL {
            assert_eq!(s.as_str().parse::<JobState>(), Ok(*s));
        }
        for s in JobSource::ALL {
            assert_eq!(s.as_str().parse::<JobSource>(), Ok(*s));
        }
        assert!("bogus".parse::<JobState>().is_err());
        assert!(JobState::Queued.is_active() && JobState::Running.is_active());
        assert!(!JobState::Succeeded.is_active() && !JobState::Failed.is_active());
        assert!(!JobState::Cancelled.is_active());
    }

    fn kind_strategy() -> impl Strategy<Value = FailureKind> {
        proptest::sample::select(FailureKind::ALL.to_vec())
    }

    proptest! {
        /// Whatever the failure sequence, a job ends up either queued for the
        /// future or failed; the attempt counter never exceeds the budget, and a
        /// terminal job is never silently revived.
        #[test]
        fn failure_sequences_respect_budget(
            kinds in proptest::collection::vec(kind_strategy(), 1..40),
            jitter in 0.0f64..=1.0,
        ) {
            let mut job = running();
            let mut now = 10;
            for kind in kinds {
                let res = job.fail(t(now), kind, "e".into(), &POLICY, jitter).unwrap();
                prop_assert!(job.attempt <= POLICY.max_attempts);
                match res {
                    Resolution::Retrying { at } => {
                        prop_assert_eq!(job.state, JobState::Queued);
                        prop_assert!(at >= t(now));
                        prop_assert_eq!(job.next_attempt_at, at);
                        now += 1;
                        job.start(t(now)).unwrap();
                    }
                    Resolution::GaveUp => {
                        prop_assert_eq!(job.state, JobState::Failed);
                        prop_assert!(job.start(t(now)).is_err());
                        break;
                    }
                }
            }
        }
    }
}
