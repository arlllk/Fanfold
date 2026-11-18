//! Contract test suites for ports.
//!
//! Each function takes a factory producing a *fresh, empty* implementation and
//! asserts the behaviour the application relies on. Run it against the in-memory
//! fake (here) and against every real adapter.

use std::{future::Future, sync::Arc, time::Duration};

use fanfold_app::{
    error::RepoError,
    ports::{
        BookCatalog, BookRecord, CursorStore, EnqueueOutcome, Event, EventSource, JobCounts,
        JobFilter, JobRepository, LogEntry, LogLevel, MetadataStore, SessionStore,
    },
};
use fanfold_domain::{
    Timestamp,
    failure::FailureKind,
    job::{Job, JobId, JobSource, JobState, Target},
    metadata::{
        Author, Chapter, FollowSource, SeriesFacts, SeriesMetadata, SeriesPart, Tag, TagKind,
        WorkFacts, WorkMetadata, WorkStats,
    },
    retry::RetryPolicy,
    work::{SeriesId, WorkId},
};

fn t(secs: i64) -> Timestamp {
    Timestamp::from_second(secs).unwrap()
}

fn work(n: u64) -> WorkId {
    WorkId::new(n).unwrap()
}

const POLICY: RetryPolicy = RetryPolicy {
    max_attempts: 3,
    base: Duration::from_secs(60),
    cap: Duration::from_secs(3600),
};

/// Run every [`JobRepository`] contract against fresh instances from `make`.
pub async fn job_repository<F, Fut>(make: F)
where
    F: Fn() -> Fut,
    Fut: Future<Output = Arc<dyn JobRepository>>,
{
    enqueue_creates_a_queued_job(&*make().await).await;
    enqueue_is_idempotent_while_active(&*make().await).await;
    finished_work_can_be_enqueued_again(&*make().await).await;
    claim_respects_due_time_and_order(&*make().await).await;
    claim_marks_running_exactly_once(&*make().await).await;
    save_and_get_roundtrip(&*make().await).await;
    save_of_unknown_job_is_not_found(&*make().await).await;
    a_work_and_a_series_with_the_same_number_are_different_targets(&*make().await).await;
    a_forced_retry_survives_storage_and_claiming(&*make().await).await;
    recover_requeues_only_running_jobs(&*make().await).await;
    list_matches_the_filter_definitions_and_pages(&*make().await).await;
    counts_agree_with_the_listing(&*make().await).await;
    save_rejects_a_second_active_job_for_one_work(&*make().await).await;
    logs_are_per_job_and_ordered(&*make().await).await;
    concurrent_claims_never_double_deliver(make().await).await;
}

async fn enqueue_creates_a_queued_job(repo: &dyn JobRepository) {
    let EnqueueOutcome::Created(job) = repo
        .enqueue(Target::Work(work(1)), JobSource::Mail, t(10))
        .await
        .unwrap()
    else {
        panic!("first enqueue must create");
    };
    assert_eq!(job.state, JobState::Queued);
    assert_eq!(
        (job.attempt, job.next_attempt_at, job.created_at),
        (0, t(10), t(10))
    );
    assert_eq!(job.source, JobSource::Mail);
    assert_eq!(repo.get(job.id).await.unwrap(), Some(job));
    assert_eq!(repo.get(JobId::new(9_999_999)).await.unwrap(), None);
}

async fn enqueue_is_idempotent_while_active(repo: &dyn JobRepository) {
    let EnqueueOutcome::Created(first) = repo
        .enqueue(Target::Work(work(1)), JobSource::Mail, t(10))
        .await
        .unwrap()
    else {
        panic!("must create");
    };
    assert_eq!(
        repo.enqueue(Target::Work(work(1)), JobSource::Manual, t(11))
            .await
            .unwrap(),
        EnqueueOutcome::AlreadyActive(first.id),
        "queued duplicate"
    );
    let running = repo.claim_next(t(12)).await.unwrap().unwrap();
    assert_eq!(running.id, first.id);
    assert_eq!(
        repo.enqueue(Target::Work(work(1)), JobSource::Mail, t(13))
            .await
            .unwrap(),
        EnqueueOutcome::AlreadyActive(first.id),
        "running duplicate"
    );
    let EnqueueOutcome::Created(other) = repo
        .enqueue(Target::Work(work(2)), JobSource::Mail, t(14))
        .await
        .unwrap()
    else {
        panic!("a different work is independent");
    };
    assert_ne!(other.id, first.id);
}

async fn finished_work_can_be_enqueued_again(repo: &dyn JobRepository) {
    let EnqueueOutcome::Created(_) = repo
        .enqueue(Target::Work(work(1)), JobSource::Mail, t(1))
        .await
        .unwrap()
    else {
        panic!("must create");
    };
    let mut job = repo.claim_next(t(2)).await.unwrap().unwrap();
    job.succeed(t(3)).unwrap();
    repo.save(&job).await.unwrap();
    let EnqueueOutcome::Created(again) = repo
        .enqueue(Target::Work(work(1)), JobSource::Mail, t(4))
        .await
        .unwrap()
    else {
        panic!("a finished job must not block a new one (an author may update a work)");
    };
    assert_ne!(again.id, job.id);
}

async fn claim_respects_due_time_and_order(repo: &dyn JobRepository) {
    assert_eq!(repo.claim_next(t(100)).await.unwrap(), None, "empty queue");
    let later = unwrap_created(
        repo.enqueue(Target::Work(work(1)), JobSource::Mail, t(50))
            .await
            .unwrap(),
    );
    let sooner = unwrap_created(
        repo.enqueue(Target::Work(work(2)), JobSource::Mail, t(20))
            .await
            .unwrap(),
    );
    let tied = unwrap_created(
        repo.enqueue(Target::Work(work(3)), JobSource::Mail, t(20))
            .await
            .unwrap(),
    );

    assert_eq!(
        repo.claim_next(t(10)).await.unwrap(),
        None,
        "nothing is due yet"
    );
    assert_eq!(
        repo.claim_next(t(30)).await.unwrap().unwrap().id,
        sooner.id,
        "oldest due first"
    );
    assert_eq!(
        repo.claim_next(t(30)).await.unwrap().unwrap().id,
        tied.id,
        "ties go by id"
    );
    assert_eq!(
        repo.claim_next(t(30)).await.unwrap(),
        None,
        "the later job is not due"
    );
    assert_eq!(repo.claim_next(t(50)).await.unwrap().unwrap().id, later.id);
}

async fn claim_marks_running_exactly_once(repo: &dyn JobRepository) {
    let queued = unwrap_created(
        repo.enqueue(Target::Work(work(1)), JobSource::Mail, t(1))
            .await
            .unwrap(),
    );
    let claimed = repo.claim_next(t(5)).await.unwrap().unwrap();
    assert_eq!(claimed.id, queued.id);
    assert_eq!(claimed.state, JobState::Running);
    assert_eq!(claimed.updated_at, t(5));
    assert_eq!(
        repo.get(queued.id).await.unwrap().unwrap().state,
        JobState::Running
    );
    assert_eq!(
        repo.claim_next(t(6)).await.unwrap(),
        None,
        "a running job is not claimable"
    );
}

async fn save_and_get_roundtrip(repo: &dyn JobRepository) {
    unwrap_created(
        repo.enqueue(Target::Work(work(1)), JobSource::Manual, t(1))
            .await
            .unwrap(),
    );
    let mut job = repo.claim_next(t(2)).await.unwrap().unwrap();
    job.fail(
        t(3),
        FailureKind::Transient,
        "boom: \"quoted\" \u{1F4A5}\nline2".into(),
        &POLICY,
        0.5,
    )
    .unwrap();
    repo.save(&job).await.unwrap();
    let stored = repo.get(job.id).await.unwrap().unwrap();
    assert_eq!(stored, job);
    assert_eq!(stored.state, JobState::Queued);
    assert_eq!(stored.attempt, 1);
    assert_eq!(
        stored.last_error.as_deref(),
        Some("boom: \"quoted\" \u{1F4A5}\nline2")
    );
    assert!(
        stored.next_attempt_at > t(3),
        "back-off schedules the retry in the future"
    );
    assert_eq!(
        repo.claim_next(t(3)).await.unwrap(),
        None,
        "not due until the back-off elapses"
    );
    assert!(
        repo.claim_next(stored.next_attempt_at)
            .await
            .unwrap()
            .is_some()
    );
}

async fn a_forced_retry_survives_storage_and_claiming(repo: &dyn JobRepository) {
    let mut job = unwrap_created(
        repo.enqueue(Target::Work(work(1)), JobSource::Mail, t(1))
            .await
            .unwrap(),
    );
    assert!(!job.force, "a new job is not forced");
    job = repo.claim_next(t(2)).await.unwrap().unwrap();
    job.succeed(t(3)).unwrap();
    repo.save(&job).await.unwrap();

    job.force_retry(t(4)).unwrap();
    repo.save(&job).await.unwrap();
    assert_eq!(repo.get(job.id).await.unwrap().unwrap(), job);
    let mut claimed = repo.claim_next(t(4)).await.unwrap().unwrap();
    assert!(claimed.force, "the worker must see the flag");

    claimed.succeed(t(5)).unwrap();
    repo.save(&claimed).await.unwrap();
    assert!(!repo.get(job.id).await.unwrap().unwrap().force);
}

async fn a_work_and_a_series_with_the_same_number_are_different_targets(repo: &dyn JobRepository) {
    let series = Target::Series(SeriesId::new(1).unwrap());
    let as_work = unwrap_created(
        repo.enqueue(Target::Work(work(1)), JobSource::Mail, t(1))
            .await
            .unwrap(),
    );
    let as_series = unwrap_created(repo.enqueue(series, JobSource::Manual, t(2)).await.unwrap());
    assert_eq!(
        repo.get(as_series.id).await.unwrap().unwrap().target,
        series
    );
    assert_eq!(
        repo.enqueue(series, JobSource::Manual, t(3)).await.unwrap(),
        EnqueueOutcome::AlreadyActive(as_series.id)
    );
    let first = repo.claim_next(t(5)).await.unwrap().unwrap();
    assert_eq!(
        (first.id, first.target),
        (as_work.id, Target::Work(work(1)))
    );
}

async fn save_of_unknown_job_is_not_found(repo: &dyn JobRepository) {
    let ghost = fanfold_domain::job::Job::new(JobId::new(424_242), work(9), JobSource::Mail, t(1));
    assert_eq!(repo.save(&ghost).await, Err(RepoError::NotFound(ghost.id)));
}

async fn recover_requeues_only_running_jobs(repo: &dyn JobRepository) {
    let orphan = unwrap_created(
        repo.enqueue(Target::Work(work(1)), JobSource::Mail, t(1))
            .await
            .unwrap(),
    );
    let waiting = unwrap_created(
        repo.enqueue(Target::Work(work(2)), JobSource::Mail, t(500))
            .await
            .unwrap(),
    );
    let mut done = unwrap_created(
        repo.enqueue(Target::Work(work(3)), JobSource::Mail, t(2))
            .await
            .unwrap(),
    );
    assert_eq!(repo.claim_next(t(10)).await.unwrap().unwrap().id, orphan.id);
    done = {
        let claimed = repo.claim_next(t(10)).await.unwrap().unwrap();
        assert_eq!(claimed.id, done.id);
        let mut claimed = claimed;
        claimed.succeed(t(11)).unwrap();
        repo.save(&claimed).await.unwrap();
        claimed
    };

    assert_eq!(repo.recover_orphans(t(20)).await.unwrap(), 1);
    let revived = repo.get(orphan.id).await.unwrap().unwrap();
    assert_eq!(
        (revived.state, revived.next_attempt_at),
        (JobState::Queued, t(20))
    );
    assert_eq!(
        repo.get(waiting.id).await.unwrap().unwrap().state,
        JobState::Queued
    );
    assert_eq!(
        repo.get(done.id).await.unwrap().unwrap().state,
        JobState::Succeeded
    );
    assert_eq!(repo.recover_orphans(t(21)).await.unwrap(), 0, "idempotent");
}

/// Build a repository holding one job in every interesting state.
async fn populate(repo: &dyn JobRepository) -> Vec<JobId> {
    let mut ids = Vec::new();
    let mut at = 10;
    let mut next = |step: i64| {
        at += step;
        t(at)
    };
    // queued, never failed
    ids.push(
        unwrap_created(
            repo.enqueue(Target::Work(work(1)), JobSource::Mail, next(1))
                .await
                .unwrap(),
        )
        .id,
    );
    // succeeded
    let id = unwrap_created(
        repo.enqueue(Target::Work(work(2)), JobSource::Mail, next(1))
            .await
            .unwrap(),
    )
    .id;
    let mut job = claim_specific(repo, id, next(1)).await;
    job.succeed(next(1)).unwrap();
    repo.save(&job).await.unwrap();
    ids.push(id);
    // failed for good
    let id = unwrap_created(
        repo.enqueue(Target::Work(work(3)), JobSource::Manual, next(1))
            .await
            .unwrap(),
    )
    .id;
    let mut job = claim_specific(repo, id, next(1)).await;
    job.fail(next(1), FailureKind::Permanent, "gone".into(), &POLICY, 0.5)
        .unwrap();
    repo.save(&job).await.unwrap();
    ids.push(id);
    // queued again after a failure
    let id = unwrap_created(
        repo.enqueue(Target::Work(work(4)), JobSource::Mail, next(1))
            .await
            .unwrap(),
    )
    .id;
    let mut job = claim_specific(repo, id, next(1)).await;
    job.fail(next(1), FailureKind::Transient, "oops".into(), &POLICY, 0.5)
        .unwrap();
    repo.save(&job).await.unwrap();
    ids.push(id);
    // running
    let id = unwrap_created(
        repo.enqueue(Target::Work(work(5)), JobSource::Mail, next(1))
            .await
            .unwrap(),
    )
    .id;
    claim_specific(repo, id, next(1)).await;
    ids.push(id);
    // cancelled
    let id = unwrap_created(
        repo.enqueue(Target::Work(work(6)), JobSource::Mail, next(1))
            .await
            .unwrap(),
    )
    .id;
    let mut job = repo.get(id).await.unwrap().unwrap();
    job.cancel(next(1)).unwrap();
    repo.save(&job).await.unwrap();
    ids.push(id);
    ids
}

/// Claim until `id` comes up, putting the others back untouched.
async fn claim_specific(repo: &dyn JobRepository, id: JobId, now: Timestamp) -> Job {
    let mut others = Vec::new();
    let found = loop {
        let job = repo
            .claim_next(now)
            .await
            .unwrap()
            .expect("target job must be due");
        if job.id == id {
            break job;
        }
        others.push(job);
    };
    for mut other in others {
        let at = other.updated_at;
        other.requeue_orphan(at).unwrap();
        repo.save(&other).await.unwrap();
    }
    found
}

async fn list_matches_the_filter_definitions_and_pages(repo: &dyn JobRepository) {
    let ids = populate(repo).await;
    let mut all = Vec::new();
    for id in &ids {
        all.push(repo.get(*id).await.unwrap().unwrap());
    }
    all.sort_by_key(|j| std::cmp::Reverse((j.updated_at, j.id)));

    for filter in JobFilter::ALL {
        let expected: Vec<_> = all
            .iter()
            .filter(|j| filter.matches(j))
            .map(|j| j.id)
            .collect();
        let got: Vec<_> = repo
            .list(filter, 100, 0)
            .await
            .unwrap()
            .iter()
            .map(|j| j.id)
            .collect();
        assert_eq!(got, expected, "filter {filter:?}");
    }
    let everything: Vec<_> = all.iter().map(|j| j.id).collect();
    let page = |limit, offset| async move {
        repo.list(JobFilter::All, limit, offset)
            .await
            .unwrap()
            .iter()
            .map(|j| j.id)
            .collect::<Vec<_>>()
    };
    assert_eq!(page(2, 0).await, everything[..2]);
    assert_eq!(page(2, 2).await, everything[2..4]);
    assert_eq!(page(2, 4).await, everything[4..]);
    assert_eq!(page(100, 6).await, Vec::new(), "past the end");
    assert_eq!(page(0, 0).await, Vec::new(), "limit 0");
}

async fn counts_agree_with_the_listing(repo: &dyn JobRepository) {
    assert_eq!(repo.counts().await.unwrap(), JobCounts::default());
    populate(repo).await;
    let len = |filter| async move { repo.list(filter, 100, 0).await.unwrap().len() as u64 };
    let counts = repo.counts().await.unwrap();
    assert_eq!(counts.retrying, len(JobFilter::Retrying).await);
    assert_eq!(counts.failed, len(JobFilter::Failed).await);
    assert_eq!(counts.succeeded, len(JobFilter::Succeeded).await);
    assert_eq!(
        counts.queued + counts.running + counts.retrying,
        len(JobFilter::Active).await
    );
    assert_eq!(
        (
            counts.queued,
            counts.running,
            counts.retrying,
            counts.succeeded,
            counts.failed,
            counts.cancelled
        ),
        (1, 1, 1, 1, 1, 1)
    );
}

async fn save_rejects_a_second_active_job_for_one_work(repo: &dyn JobRepository) {
    let first = unwrap_created(
        repo.enqueue(Target::Work(work(1)), JobSource::Mail, t(1))
            .await
            .unwrap(),
    );
    let mut failed = claim_specific(repo, first.id, t(2)).await;
    failed
        .fail(t(3), FailureKind::Permanent, "x".into(), &POLICY, 0.5)
        .unwrap();
    repo.save(&failed).await.unwrap();
    // The work is queued again (say, by a new email) while the old job is Failed.
    let second = unwrap_created(
        repo.enqueue(Target::Work(work(1)), JobSource::Mail, t(4))
            .await
            .unwrap(),
    );

    // Retrying the old job would make two active jobs for one work.
    failed.retry(t(5)).unwrap();
    assert!(matches!(
        repo.save(&failed).await,
        Err(RepoError::Conflict(_))
    ));
    assert_eq!(
        repo.get(first.id).await.unwrap().unwrap().state,
        JobState::Failed,
        "unchanged"
    );
    assert_eq!(
        repo.get(second.id).await.unwrap().unwrap().state,
        JobState::Queued
    );
}

/// Run the [`EventSource`] contract: changes are announced.
pub async fn event_source<F, Fut>(make: F)
where
    F: Fn() -> Fut,
    Fut: Future<Output = (Arc<dyn JobRepository>, Arc<dyn EventSource>)>,
{
    use tokio::sync::broadcast::{Receiver, error::RecvError};

    async fn expect(rx: &mut Receiver<Event>, wanted: Event) {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        loop {
            match tokio::time::timeout_at(deadline, rx.recv()).await {
                Ok(Ok(event)) if event == wanted => return,
                Ok(Ok(_) | Err(RecvError::Lagged(_))) => {}
                other => panic!("never saw {wanted:?}: {other:?}"),
            }
        }
    }

    let (repo, events) = make().await;
    let mut rx = events.subscribe();

    let job = unwrap_created(
        repo.enqueue(Target::Work(work(1)), JobSource::Mail, t(1))
            .await
            .unwrap(),
    );
    expect(&mut rx, Event::JobChanged(job.id)).await;

    let entry = LogEntry {
        at: t(2),
        attempt: 0,
        level: LogLevel::Info,
        message: "hi".into(),
    };
    repo.append_log(job.id, &entry).await.unwrap();
    expect(&mut rx, Event::LogAppended(job.id)).await;

    let mut claimed = repo.claim_next(t(3)).await.unwrap().unwrap();
    expect(&mut rx, Event::JobChanged(job.id)).await;
    claimed.succeed(t(4)).unwrap();
    repo.save(&claimed).await.unwrap();
    expect(&mut rx, Event::JobChanged(job.id)).await;
}

async fn logs_are_per_job_and_ordered(repo: &dyn JobRepository) {
    let a = unwrap_created(
        repo.enqueue(Target::Work(work(1)), JobSource::Mail, t(1))
            .await
            .unwrap(),
    );
    let b = unwrap_created(
        repo.enqueue(Target::Work(work(2)), JobSource::Mail, t(1))
            .await
            .unwrap(),
    );
    let entry = |secs, message: &str| LogEntry {
        at: t(secs),
        attempt: 2,
        level: LogLevel::Warn,
        message: message.to_owned(),
    };
    repo.append_log(a.id, &entry(1, "first")).await.unwrap();
    repo.append_log(b.id, &entry(2, "other job")).await.unwrap();
    repo.append_log(a.id, &entry(3, "second \u{2603}\nmultiline"))
        .await
        .unwrap();
    assert_eq!(
        repo.logs(a.id).await.unwrap(),
        [entry(1, "first"), entry(3, "second \u{2603}\nmultiline")]
    );
    assert_eq!(repo.logs(b.id).await.unwrap(), [entry(2, "other job")]);
    assert_eq!(repo.logs(JobId::new(777_777)).await.unwrap(), Vec::new());
    assert!(
        repo.append_log(JobId::new(777_777), &entry(4, "x"))
            .await
            .is_err()
    );
}

async fn concurrent_claims_never_double_deliver(repo: Arc<dyn JobRepository>) {
    const JOBS: u64 = 40;
    for n in 1..=JOBS {
        unwrap_created(
            repo.enqueue(Target::Work(work(n)), JobSource::Mail, t(1))
                .await
                .unwrap(),
        );
    }
    let mut tasks = Vec::new();
    for _ in 0..8 {
        let repo = Arc::clone(&repo);
        tasks.push(tokio::spawn(async move {
            let mut mine = Vec::new();
            while let Some(job) = repo.claim_next(t(100)).await.unwrap() {
                mine.push(job.id);
            }
            mine
        }));
    }
    let mut all = Vec::new();
    for task in tasks {
        all.extend(task.await.unwrap());
    }
    all.sort();
    let before = all.len();
    all.dedup();
    assert_eq!(before, all.len(), "a job was delivered twice");
    assert_eq!(all.len() as u64, JOBS, "a job was lost");
}

fn unwrap_created(outcome: EnqueueOutcome) -> fanfold_domain::job::Job {
    match outcome {
        EnqueueOutcome::Created(job) => job,
        EnqueueOutcome::AlreadyActive(id) => panic!("expected a new job, found active {id}"),
    }
}

/// Run every [`BookCatalog`] contract against fresh instances from `make`.
pub async fn book_catalog<F, Fut>(make: F)
where
    F: Fn() -> Fut,
    Fut: Future<Output = Arc<dyn BookCatalog>>,
{
    let removable = make().await;
    assert!(
        !removable.remove(work(3)).await.unwrap(),
        "nothing to remove"
    );
    removable
        .upsert(&BookRecord {
            work: work(3),
            file_name: "single/A/T 3.epub".into(),
            title: "T".into(),
            updated_at: t(1),
        })
        .await
        .unwrap();
    assert!(removable.remove(work(3)).await.unwrap());
    assert_eq!(removable.get(work(3)).await.unwrap(), None);
    assert!(removable.titles(&[work(3)]).await.unwrap().is_empty());

    let catalog = make().await;
    assert_eq!(catalog.get(work(1)).await.unwrap(), None);
    let record = BookRecord {
        work: work(1),
        file_name: "A Title [1].epub".into(),
        title: "A Title".into(),
        updated_at: t(5),
    };
    catalog.upsert(&record).await.unwrap();
    assert_eq!(catalog.get(work(1)).await.unwrap(), Some(record.clone()));
    assert_eq!(catalog.get(work(2)).await.unwrap(), None);

    let retitled = BookRecord {
        title: "New Title".into(),
        updated_at: t(9),
        ..record.clone()
    };
    catalog.upsert(&retitled).await.unwrap();
    assert_eq!(
        catalog.get(work(1)).await.unwrap(),
        Some(retitled.clone()),
        "upsert replaces the title"
    );

    // The path is chosen once. A later upsert naming a different file must not move
    // the book: e-readers key reading progress on it.
    let moved = BookRecord {
        file_name: "Different [1].epub".into(),
        title: "Newer".into(),
        ..retitled
    };
    catalog.upsert(&moved).await.unwrap();
    let stored = catalog.get(work(1)).await.unwrap().unwrap();
    assert_eq!(
        stored.file_name, "A Title [1].epub",
        "file_name is immutable once recorded"
    );
    assert_eq!(stored.title, "Newer");

    // Titles are looked up in bulk; unknown works are simply absent.
    let second = BookRecord {
        work: work(2),
        file_name: "Other [2].epub".into(),
        title: "Other".into(),
        updated_at: t(1),
    };
    catalog.upsert(&second).await.unwrap();
    let titles = catalog.titles(&[work(1), work(2), work(3)]).await.unwrap();
    assert_eq!(titles.len(), 2);
    assert_eq!(titles[&work(1)], "Newer");
    assert_eq!(titles[&work(2)], "Other");
    assert!(catalog.titles(&[]).await.unwrap().is_empty());
}

/// Run the [`CursorStore`] contract against fresh instances from `make`.
pub async fn cursor_store<F, Fut>(make: F)
where
    F: Fn() -> Fut,
    Fut: Future<Output = Arc<dyn CursorStore>>,
{
    let store = make().await;
    assert_eq!(
        store.get("mail").await.unwrap(),
        None,
        "unknown names are absent"
    );
    store.set("mail", "state-1").await.unwrap();
    assert_eq!(store.get("mail").await.unwrap().as_deref(), Some("state-1"));
    store
        .set("mail", "state-2 \u{1F4EC}\n\"quoted\"")
        .await
        .unwrap();
    assert_eq!(
        store.get("mail").await.unwrap().as_deref(),
        Some("state-2 \u{1F4EC}\n\"quoted\""),
        "set replaces, and values are stored verbatim"
    );
    store.set("other", "x").await.unwrap();
    assert_eq!(
        store.get("mail").await.unwrap().as_deref(),
        Some("state-2 \u{1F4EC}\n\"quoted\"")
    );
    assert_eq!(
        store.get("other").await.unwrap().as_deref(),
        Some("x"),
        "names are independent"
    );
    store.set("empty", "").await.unwrap();
    assert_eq!(
        store.get("empty").await.unwrap().as_deref(),
        Some(""),
        "an empty value is not 'absent'"
    );
}

/// Run the [`SessionStore`] contract against fresh instances from `make`.
pub async fn session_store<F, Fut>(make: F)
where
    F: Fn() -> Fut,
    Fut: Future<Output = Arc<dyn SessionStore>>,
{
    use fanfold_domain::session::{BrowserSession, Cookie};

    let at = |s: i64| Timestamp::from_second(s).unwrap();
    let cookie = |name: &str, value: &str, expires: Option<i64>| Cookie {
        name: name.into(),
        value: value.into(),
        domain: ".archiveofourown.org".into(),
        path: "/".into(),
        expires: expires.map(at),
        secure: true,
        http_only: expires.is_some(),
    };
    let session = |ua: &str, cookies: Vec<Cookie>, updated: i64| BrowserSession {
        user_agent: ua.into(),
        cookies,
        updated_at: at(updated),
    };

    let store = make().await;
    assert_eq!(store.load().await.unwrap(), None, "no session at first");
    assert!(!store.clear().await.unwrap(), "nothing to clear");
    assert!(
        !store
            .refresh(&session("ua", vec![], 2), at(1))
            .await
            .unwrap(),
        "refreshing a session that does not exist stores nothing"
    );
    assert_eq!(store.load().await.unwrap(), None);

    let first = session(
        "Mozilla/5.0 \u{1F98A}",
        vec![
            cookie("cf_clearance", "tok\"en'1", Some(2_000_000_000)),
            cookie("_otwarchive_session", "s", None),
        ],
        1_000,
    );
    store.save(&first).await.unwrap();
    assert_eq!(
        store.load().await.unwrap(),
        Some(first.clone()),
        "everything round-trips: values, expiry, flags, unicode"
    );

    let rotated = session(
        "Mozilla/5.0 \u{1F98A}",
        vec![cookie("cf_clearance", "new", Some(2_100_000_000))],
        2_000,
    );
    assert!(
        !store.refresh(&rotated, at(999)).await.unwrap(),
        "a refresh based on an out-of-date session is refused"
    );
    assert_eq!(store.load().await.unwrap(), Some(first.clone()));
    assert!(store.refresh(&rotated, at(1_000)).await.unwrap());
    assert_eq!(store.load().await.unwrap(), Some(rotated.clone()));
    assert!(
        !store.refresh(&rotated, at(1_000)).await.unwrap(),
        "and the same base cannot be used twice"
    );

    let imported = session("other", vec![cookie("a", "b", None)], 3_000);
    store.save(&imported).await.unwrap();
    assert_eq!(
        store.load().await.unwrap(),
        Some(imported),
        "an import replaces unconditionally"
    );
    assert!(store.clear().await.unwrap());
    assert_eq!(store.load().await.unwrap(), None);
    assert!(!store.clear().await.unwrap());
}

// -------------------------------------------------------------------- metadata

fn series_id(n: u64) -> SeriesId {
    SeriesId::new(n).unwrap()
}

/// A realistic observation of work 94243666 (The Malfoid Saga, part 2).
#[must_use]
pub fn observed_work(n: u64) -> WorkMetadata {
    WorkMetadata {
        work: work(n),
        facts: WorkFacts {
            title: "Harry Potter and the Girl with the Diary".into(),
            byline: "TheBrokenPokeman".into(),
            summary_html: "<p>It's now Harry Potter's second year.</p>".into(),
            language: "English".into(),
            rating: "Teen And Up Audiences".into(),
            restricted: false,
            status: "In-Progress".into(),
            published_on: Some(jiff_date(2026, 10, 8)),
            updated_on: Some(jiff_date(2026, 10, 20)),
            chapters_posted: 2,
            chapters_total: None,
            words: 179_890,
        },
        authors: vec![Author {
            user: "TheBrokenPokeman".into(),
            pseud: "TheBrokenPokeman".into(),
        }],
        tags: vec![
            Tag {
                kind: TagKind::Fandom,
                name: "Harry Potter - J. K. Rowling".into(),
            },
            Tag {
                kind: TagKind::Relationship,
                name: "Draco Malfoy/Harry Potter".into(),
            },
            Tag {
                kind: TagKind::Freeform,
                name: "Enemies to Lovers".into(),
            },
        ],
        series: vec![SeriesPart {
            id: series_id(6_704_161),
            name: "The Malfoid Saga".into(),
            part: Some(2),
        }],
        chapters: vec![
            Chapter {
                number: 1,
                title: "1. Making No Noise".into(),
                ao3_id: Some(11),
                posted_on: Some(jiff_date(2026, 10, 8)),
            },
            Chapter {
                number: 2,
                title: "2. The Diary".into(),
                ao3_id: Some(12),
                posted_on: Some(jiff_date(2026, 10, 9)),
            },
        ],
        stats: WorkStats {
            hits: Some(117_707),
            kudos: Some(2874),
            comments: Some(1907),
            bookmarks: Some(235),
        },
        raw: serde_json::json!({"title": "Harry Potter and the Girl with the Diary", "numWords": "179,890"}),
    }
}

fn jiff_date(y: i16, m: i8, d: i8) -> fanfold_domain::metadata::Date {
    fanfold_domain::metadata::Date::new(y, m, d).unwrap()
}

/// Run every [`MetadataStore`] contract against fresh instances from `make`.
pub async fn metadata_store<F, Fut>(make: F)
where
    F: Fn() -> Fut,
    Fut: Future<Output = Arc<dyn MetadataStore>>,
{
    versions_are_written_only_for_changes(&*make().await).await;
    spans_follow_what_the_site_shows(&*make().await).await;
    series_are_followed_once_and_named(&*make().await).await;
    a_series_can_be_unfollowed_and_knows_its_members(&*make().await).await;
    a_series_folder_is_chosen_once(&*make().await).await;
}

async fn versions_are_written_only_for_changes(store: &dyn MetadataStore) {
    let first = observed_work(5);
    assert_eq!(store.work_state(work(5)).await.unwrap(), None);
    assert!(
        store
            .record_work(&first, t(10), None)
            .await
            .unwrap()
            .new_version
    );
    assert_eq!(
        store.work_state(work(5)).await.unwrap(),
        Some(first.state())
    );

    let mut busier = first.clone();
    busier.stats.hits = Some(200_000);
    busier.raw = serde_json::json!({"hits": "200,000"});
    assert!(
        !store
            .record_work(&busier, t(20), None)
            .await
            .unwrap()
            .new_version,
        "statistics and the raw answer alone are not a change"
    );

    let mut updated = first.clone();
    updated.facts.chapters_posted = 3;
    updated.facts.restricted = true;
    assert!(
        store
            .record_work(&updated, t(30), None)
            .await
            .unwrap()
            .new_version
    );
    assert_eq!(
        store.work_state(work(5)).await.unwrap(),
        Some(updated.state())
    );
    assert_eq!(
        store.work_state(work(6)).await.unwrap(),
        None,
        "other works are untouched"
    );
}

async fn spans_follow_what_the_site_shows(store: &dyn MetadataStore) {
    let first = observed_work(5);
    store.record_work(&first, t(10), None).await.unwrap();

    let mut retagged = first.clone();
    retagged.tags.remove(2);
    retagged.tags.insert(
        0,
        Tag {
            kind: TagKind::Freeform,
            name: "Yandere".into(),
        },
    );
    retagged.chapters[1].title = "2. The Diary (edited)".into();
    retagged.series[0].part = Some(3);
    assert!(
        store
            .record_work(&retagged, t(20), None)
            .await
            .unwrap()
            .new_version
    );
    let state = store.work_state(work(5)).await.unwrap().unwrap();
    let mut tags = state.tags.clone();
    tags.sort_by(|a, b| a.name.cmp(&b.name));
    let mut expected = retagged.tags.clone();
    expected.sort_by(|a, b| a.name.cmp(&b.name));
    assert_eq!(
        tags, expected,
        "the removed tag is closed, the new one open"
    );
    assert_eq!(state.chapters[1].title, "2. The Diary (edited)");
    assert_eq!(state.chapters.len(), 2);
    assert_eq!(state.series, retagged.series);

    // Reordering is not a change.
    let mut reordered = retagged.clone();
    reordered.tags.reverse();
    assert!(
        !store
            .record_work(&reordered, t(30), None)
            .await
            .unwrap()
            .new_version
    );
}

async fn series_are_followed_once_and_named(store: &dyn MetadataStore) {
    let (a, b, c) = (series_id(1), series_id(2), series_id(3));
    assert_eq!(
        store.followed(&[a, b, c]).await.unwrap(),
        Vec::<SeriesId>::new()
    );
    store
        .follow_series(b, FollowSource::Mail, t(5))
        .await
        .unwrap();
    store
        .follow_series(b, FollowSource::Manual, t(6))
        .await
        .unwrap();
    store
        .follow_series(a, FollowSource::Manual, t(7))
        .await
        .unwrap();
    assert_eq!(
        store.followed(&[c, b, a]).await.unwrap(),
        [b, a],
        "in the order asked"
    );

    let listing = SeriesMetadata {
        series: a,
        facts: SeriesFacts {
            name: "The Malfoid Saga".into(),
            summary_html: String::new(),
            complete: Some(false),
            works: vec![work(1), work(2)],
        },
        raw: serde_json::json!({"name": "The Malfoid Saga"}),
    };
    assert!(
        store
            .record_series(&listing, t(10), None)
            .await
            .unwrap()
            .new_version
    );
    assert!(
        !store
            .record_series(&listing, t(11), None)
            .await
            .unwrap()
            .new_version
    );
    let mut grown = listing.clone();
    grown.facts.works.push(work(3));
    assert!(
        store
            .record_series(&grown, t(12), None)
            .await
            .unwrap()
            .new_version
    );

    // Series 6704161 was never listed, but a work's page named it.
    store
        .record_work(&observed_work(5), t(13), None)
        .await
        .unwrap();
    let names = store
        .series_names(&[a, series_id(6_704_161), c])
        .await
        .unwrap();
    assert_eq!(names.get(&a).map(String::as_str), Some("The Malfoid Saga"));
    assert_eq!(
        names.get(&series_id(6_704_161)).map(String::as_str),
        Some("The Malfoid Saga")
    );
    assert_eq!(names.get(&c), None);
}

async fn a_series_can_be_unfollowed_and_knows_its_members(store: &dyn MetadataStore) {
    let saga = series_id(6_704_161);
    assert!(
        !store.unfollow_series(saga).await.unwrap(),
        "never followed"
    );
    store
        .follow_series(saga, FollowSource::Manual, t(1))
        .await
        .unwrap();
    // Work 5 names the series on its page (observed_work); 7 and 5 are in its listing.
    store
        .record_work(&observed_work(5), t(2), None)
        .await
        .unwrap();
    store
        .record_series(
            &SeriesMetadata {
                series: saga,
                facts: SeriesFacts {
                    name: "The Malfoid Saga".into(),
                    summary_html: String::new(),
                    complete: None,
                    works: vec![work(7), work(5)],
                },
                raw: serde_json::Value::Null,
            },
            t(3),
            None,
        )
        .await
        .unwrap();
    let mut members = store.series_members(saga).await.unwrap();
    members.sort();
    assert_eq!(
        members,
        [work(5), work(7)],
        "page and listing, no duplicates"
    );
    assert_eq!(
        store.series_members(series_id(1)).await.unwrap(),
        Vec::<WorkId>::new()
    );

    assert!(store.unfollow_series(saga).await.unwrap());
    assert_eq!(
        store.followed(&[saga]).await.unwrap(),
        Vec::<SeriesId>::new()
    );
    assert!(
        !store.unfollow_series(saga).await.unwrap(),
        "already unfollowed"
    );
    store
        .follow_series(saga, FollowSource::Mail, t(9))
        .await
        .unwrap();
    assert_eq!(
        store.followed(&[saga]).await.unwrap(),
        [saga],
        "it can be followed again"
    );
}

async fn a_series_folder_is_chosen_once(store: &dyn MetadataStore) {
    let s = series_id(9);
    assert_eq!(
        store.claim_series_folder(s, "First 9").await.unwrap(),
        "First 9"
    );
    assert_eq!(
        store.claim_series_folder(s, "Renamed 9").await.unwrap(),
        "First 9"
    );
    assert_eq!(
        store.followed(&[s]).await.unwrap(),
        Vec::<SeriesId>::new(),
        "choosing a folder does not follow"
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::InMemoryJobs;

    #[tokio::test]
    async fn in_memory_session_store_honours_the_contract() {
        session_store(|| async {
            Arc::new(crate::memory::InMemorySession::new()) as Arc<dyn SessionStore>
        })
        .await;
    }

    #[tokio::test]
    async fn in_memory_job_repository_honours_the_contract() {
        job_repository(|| async { Arc::new(InMemoryJobs::new()) as Arc<dyn JobRepository> }).await;
    }

    #[tokio::test]
    async fn in_memory_event_source_honours_the_contract() {
        event_source(|| async {
            let repo = Arc::new(InMemoryJobs::new());
            (
                repo.clone() as Arc<dyn JobRepository>,
                repo as Arc<dyn EventSource>,
            )
        })
        .await;
    }

    #[tokio::test]
    async fn in_memory_cursor_store_honours_the_contract() {
        cursor_store(|| async {
            Arc::new(crate::memory::InMemoryCursors::new()) as Arc<dyn CursorStore>
        })
        .await;
    }

    #[tokio::test]
    async fn in_memory_catalog_honours_the_contract() {
        book_catalog(|| async { Arc::new(InMemoryJobs::new()) as Arc<dyn BookCatalog> }).await;
    }

    #[tokio::test]
    async fn in_memory_metadata_store_honours_the_contract() {
        metadata_store(|| async { Arc::new(InMemoryJobs::new()) as Arc<dyn MetadataStore> }).await;
    }
}
