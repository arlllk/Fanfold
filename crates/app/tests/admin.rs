//! The use cases behind the UI's buttons, against in-memory adapters.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::clone_on_ref_ptr
)]

use std::{sync::Arc, time::Duration};

use fanfold_app::{
    admin::{AdminError, JobAdmin},
    error::RepoError,
    ports::{Clock, EnqueueOutcome, JobFilter, JobRepository},
};
use fanfold_domain::{
    failure::FailureKind,
    job::{JobId, JobState, Target},
    retry::RetryPolicy,
    work::{UrlError, WorkId},
};
use fanfold_testkit::memory::{FakeClock, InMemoryJobs};

const POLICY: RetryPolicy = RetryPolicy {
    max_attempts: 3,
    base: Duration::from_secs(60),
    cap: Duration::from_secs(3600),
};

struct Rig {
    jobs: Arc<InMemoryJobs>,
    clock: Arc<FakeClock>,
    admin: JobAdmin,
}

fn rig() -> Rig {
    let jobs = Arc::new(InMemoryJobs::new());
    let clock = Arc::new(FakeClock::at_secs(1_000));
    let admin = JobAdmin::new(jobs.clone(), clock.clone());
    Rig { jobs, clock, admin }
}

impl Rig {
    async fn queue(&self, work: u64) -> JobId {
        match self
            .admin
            .add_url(&format!("https://archiveofourown.org/works/{work}"))
            .await
            .unwrap()
        {
            EnqueueOutcome::Created(job) => job.id,
            EnqueueOutcome::AlreadyActive(id) => id,
        }
    }

    /// Run a queued job to the given terminal state.
    async fn finish(&self, id: JobId, state: JobState) {
        let mut job = self
            .jobs
            .claim_next(self.clock.now())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(job.id, id);
        let now = self.clock.now();
        match state {
            JobState::Succeeded => job.succeed(now).unwrap(),
            JobState::Failed => {
                job.fail(now, FailureKind::Permanent, "gone".into(), &POLICY, 0.5)
                    .unwrap();
            }
            other => panic!("unsupported {other}"),
        }
        self.jobs.save(&job).await.unwrap();
    }
}

#[tokio::test]
async fn adding_a_link_queues_it_once() {
    let rig = rig();
    let first = rig
        .admin
        .add_url("  https://www.archiveofourown.org/works/77/chapters/1?x=y  ")
        .await
        .unwrap();
    let EnqueueOutcome::Created(job) = first else {
        panic!("must create")
    };
    assert_eq!(
        (job.target, job.state),
        (Target::Work(WorkId::new(77).unwrap()), JobState::Queued)
    );
    assert_eq!(job.source, fanfold_domain::job::JobSource::Manual);

    let again = rig
        .admin
        .add_url("https://archiveofourown.org/works/77")
        .await
        .unwrap();
    assert_eq!(again, EnqueueOutcome::AlreadyActive(job.id));
}

#[tokio::test]
async fn a_series_link_queues_one_series_job_once() {
    let rig = rig();
    let link = "https://archiveofourown.org/series/2000001?page=2";
    let EnqueueOutcome::Created(job) = rig.admin.add_url(link).await.unwrap() else {
        panic!("must create")
    };
    assert_eq!(
        job.target,
        Target::Series(fanfold_domain::work::SeriesId::new(2_000_001).unwrap())
    );
    assert_eq!(
        rig.admin.add_url(link).await.unwrap(),
        EnqueueOutcome::AlreadyActive(job.id)
    );
    // A work with the same number is a different thing.
    assert!(matches!(
        rig.admin
            .add_url("https://archiveofourown.org/works/2000001")
            .await
            .unwrap(),
        EnqueueOutcome::Created(_)
    ));
}

#[tokio::test]
async fn bad_links_are_rejected_with_the_reason() {
    let rig = rig();
    for (input, reason) in [
        ("https://example.com/works/1", UrlError::Host),
        (
            "https://archiveofourown.org/users/someone",
            UrlError::NotAWork,
        ),
        ("not a link", UrlError::Malformed),
        ("", UrlError::Malformed),
    ] {
        assert_eq!(
            rig.admin.add_url(input).await.unwrap_err(),
            AdminError::InvalidUrl(reason),
            "{input:?}"
        );
    }
    assert_eq!(rig.jobs.all_jobs(), Vec::new());
}

#[tokio::test]
async fn a_failed_job_can_be_retried_with_a_fresh_budget() {
    let rig = rig();
    let id = rig.queue(1).await;
    rig.finish(id, JobState::Failed).await;
    rig.clock.advance(Duration::from_secs(60));

    let job = rig.admin.retry(id).await.unwrap();
    assert_eq!((job.state, job.attempt), (JobState::Queued, 0));
    assert_eq!(job.next_attempt_at, rig.clock.now(), "due immediately");
    assert!(
        rig.jobs
            .claim_next(rig.clock.now())
            .await
            .unwrap()
            .is_some()
    );
}

#[tokio::test]
async fn retry_and_cancel_respect_the_job_state() {
    let rig = rig();
    let queued = rig.queue(1).await;
    // Only failed or cancelled jobs can be retried; only queued jobs cancelled.
    assert!(matches!(
        rig.admin.retry(queued).await,
        Err(AdminError::WrongState(_))
    ));
    assert_eq!(
        rig.admin.cancel(queued).await.unwrap().state,
        JobState::Cancelled
    );
    assert!(matches!(
        rig.admin.cancel(queued).await,
        Err(AdminError::WrongState(_))
    ));
    assert_eq!(
        rig.admin.retry(queued).await.unwrap().state,
        JobState::Queued,
        "cancelled jobs can be retried"
    );

    let running = rig.queue(2).await;
    let claimed = rig.jobs.claim_next(rig.clock.now()).await.unwrap().unwrap();
    assert_eq!(claimed.id, queued, "oldest first");
    let claimed = rig.jobs.claim_next(rig.clock.now()).await.unwrap().unwrap();
    assert_eq!(claimed.id, running);
    assert!(
        matches!(
            rig.admin.cancel(running).await,
            Err(AdminError::WrongState(_))
        ),
        "a running job cannot be cancelled"
    );
}

#[tokio::test]
async fn a_finished_job_can_be_forced_to_download_again() {
    let rig = rig();
    let done = rig.queue(1).await;
    rig.finish(done, JobState::Succeeded).await;
    let failed = rig.queue(2).await;
    rig.finish(failed, JobState::Failed).await;
    let old = rig.queue(3).await;
    rig.finish(old, JobState::Succeeded).await;
    rig.queue(3).await; // a fresh email queued it again
    rig.clock.advance(Duration::from_secs(60));

    for id in [done, failed] {
        let job = rig.admin.force_retry(id).await.unwrap();
        assert_eq!(
            (job.state, job.attempt, job.force),
            (JobState::Queued, 0, true)
        );
        assert_eq!(job.next_attempt_at, rig.clock.now(), "due immediately");
        assert!(rig.jobs.get(id).await.unwrap().unwrap().force, "stored");
    }
    assert!(
        matches!(
            rig.admin.force_retry(done).await,
            Err(AdminError::WrongState(_))
        ),
        "already queued"
    );

    assert_eq!(
        rig.admin.force_retry(old).await.unwrap_err(),
        AdminError::AlreadyActive(Target::Work(WorkId::new(3).unwrap()))
    );
    assert_eq!(
        rig.admin.force_retry(JobId::new(404)).await.unwrap_err(),
        AdminError::NotFound(JobId::new(404))
    );
}

#[tokio::test]
async fn unknown_jobs_are_not_found() {
    let rig = rig();
    let ghost = JobId::new(404);
    assert_eq!(
        rig.admin.retry(ghost).await.unwrap_err(),
        AdminError::NotFound(ghost)
    );
    assert_eq!(
        rig.admin.cancel(ghost).await.unwrap_err(),
        AdminError::NotFound(ghost)
    );
}

#[tokio::test]
async fn retrying_when_the_work_is_already_queued_again_is_a_conflict() {
    let rig = rig();
    let old = rig.queue(1).await;
    rig.finish(old, JobState::Failed).await;
    let newer = rig.queue(1).await; // a fresh email queued it again
    assert_ne!(old, newer);

    assert_eq!(
        rig.admin.retry(old).await.unwrap_err(),
        AdminError::AlreadyActive(Target::Work(WorkId::new(1).unwrap()))
    );
    assert_eq!(
        rig.jobs.get(old).await.unwrap().unwrap().state,
        JobState::Failed
    );
}

#[tokio::test]
async fn bulk_retry_reports_each_outcome_instead_of_failing_as_a_whole() {
    let rig = rig();
    let a = rig.queue(1).await;
    let b = rig.queue(2).await;
    let c = rig.queue(3).await;
    for id in [a, b, c] {
        rig.finish(id, JobState::Failed).await;
    }
    let _requeued = rig.queue(2).await; // makes b's retry conflict
    let ghost = JobId::new(999);

    let report = rig.admin.retry_many(&[a, b, c, ghost]).await;
    assert_eq!(report.retried, [a, c]);
    let skipped: Vec<_> = report.skipped.iter().map(|(id, _)| *id).collect();
    assert_eq!(skipped, [b, ghost]);
    assert!(
        report.skipped[0].1.contains("already queued"),
        "{:?}",
        report.skipped
    );
    assert!(
        report.skipped[1].1.contains("not found"),
        "{:?}",
        report.skipped
    );
}

#[tokio::test]
async fn retry_all_failed_touches_only_failed_jobs() {
    let rig = rig();
    let failed: Vec<_> = {
        let mut ids = Vec::new();
        for n in 1..=3 {
            let id = rig.queue(n).await;
            rig.finish(id, JobState::Failed).await;
            ids.push(id);
        }
        ids
    };
    let ok = rig.queue(10).await;
    rig.finish(ok, JobState::Succeeded).await;
    let waiting = rig.queue(11).await;

    let report = rig.admin.retry_all_failed().await.unwrap();
    let mut retried = report.retried.clone();
    retried.sort();
    assert_eq!(retried, failed);
    assert_eq!(report.skipped, Vec::new());
    assert_eq!(
        rig.jobs.get(ok).await.unwrap().unwrap().state,
        JobState::Succeeded
    );
    assert_eq!(
        rig.jobs.get(waiting).await.unwrap().unwrap().state,
        JobState::Queued
    );
    assert_eq!(
        rig.jobs.list(JobFilter::Failed, 10, 0).await.unwrap(),
        Vec::new()
    );
}

#[tokio::test]
async fn storage_errors_surface_as_repo_errors() {
    let rig = rig();
    let id = rig.queue(1).await;
    rig.jobs
        .break_with(Some(RepoError::Backend("db down".into())));
    assert!(matches!(
        rig.admin.retry(id).await,
        Err(AdminError::Repo(_))
    ));
    assert!(matches!(
        rig.admin
            .add_url("https://archiveofourown.org/works/2")
            .await,
        Err(AdminError::Repo(_))
    ));
    assert!(matches!(
        rig.admin.retry_all_failed().await,
        Err(AdminError::Repo(_))
    ));
}
