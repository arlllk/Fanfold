//! The real axum app, driven request by request against in-memory adapters.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::clone_on_ref_ptr,
    clippy::indexing_slicing
)]

use std::{
    sync::{Arc, Mutex},
    time::Duration,
};

use axum::{
    Router,
    body::Body,
    http::{HeaderMap, Method, Request, StatusCode, header},
};
use fanfold_adapter_web::{WebDeps, ops_router, ui_router};
use fanfold_app::{
    admin::JobAdmin,
    ports::{
        BookCatalog, BookRecord, Clock, EnqueueOutcome, JobRepository, LogEntry, LogLevel,
        SiteStatus, SiteStatusSource,
    },
    stats::WorkerStats,
};
use fanfold_domain::{
    failure::FailureKind,
    job::{Job, JobId, JobSource},
    retry::RetryPolicy,
    work::WorkId,
};
use fanfold_testkit::memory::{FakeClock, InMemoryJobs};
use http_body_util::BodyExt;
use tower::ServiceExt;

const NOW: i64 = 1_800_000_000;

const POLICY: RetryPolicy = RetryPolicy {
    max_attempts: 5,
    base: Duration::from_secs(60),
    cap: Duration::from_secs(3600),
};

struct FixedSite(Mutex<SiteStatus>);

impl SiteStatusSource for FixedSite {
    fn site_status(&self) -> SiteStatus {
        *self.0.lock().unwrap()
    }
}

struct Rig {
    /// Dropping the sender would end every event stream, so the rig owns it.
    stop: tokio::sync::watch::Sender<bool>,
    jobs: Arc<InMemoryJobs>,
    library: Arc<fanfold_testkit::memory::InMemoryLibrary>,
    clock: Arc<FakeClock>,
    site: Arc<FixedSite>,
    ui: Router,
    ops: Router,
}

fn rig_with_page_size(page_size: u32) -> Rig {
    let jobs = Arc::new(InMemoryJobs::new());
    let clock = Arc::new(FakeClock::at_secs(NOW));
    let site = Arc::new(FixedSite(Mutex::new(SiteStatus::Healthy)));
    let library = Arc::new(fanfold_testkit::memory::InMemoryLibrary::new());
    let (stop, stopped) = tokio::sync::watch::channel(false);
    let deps = Arc::new(WebDeps {
        jobs: jobs.clone(),
        catalog: jobs.clone(),
        metadata: jobs.clone(),
        listing: jobs.clone(),
        browser: Arc::new(fanfold_app::browse::LibraryBrowser::new(
            jobs.clone(),
            Arc::new(fanfold_adapter_search::TantivyIndexer),
        )),
        admin: Arc::new(JobAdmin::new(jobs.clone(), clock.clone())),
        library: Arc::new(fanfold_app::library::LibraryAdmin::new(
            jobs.clone(),
            jobs.clone(),
            library.clone(),
            jobs.clone(),
            clock.clone(),
        )),
        events: jobs.clone(),
        site: site.clone(),
        clock: clock.clone(),
        stats: Arc::new(WorkerStats::new()),
        mail: Arc::new(fanfold_app::stats::MailStats::new()),
        readiness: jobs.clone(),
        shutdown: stopped,
        max_attempts: 5,
        page_size,
        version: "test",
    });
    Rig {
        stop,
        jobs,
        library,
        clock,
        site,
        ui: ui_router(deps.clone()),
        ops: ops_router(deps),
    }
}

fn rig() -> Rig {
    rig_with_page_size(20)
}

struct Reply {
    status: StatusCode,
    headers: HeaderMap,
    body: String,
}

impl Reply {
    fn location(&self) -> &str {
        self.headers[header::LOCATION].to_str().unwrap()
    }
}

async fn send(
    app: &Router,
    method: Method,
    path: &str,
    headers: &[(&str, &str)],
    body: &str,
) -> Reply {
    let mut request = Request::builder().method(method).uri(path);
    for (k, v) in headers {
        request = request.header(*k, *v);
    }
    let response = app
        .clone()
        .oneshot(request.body(Body::from(body.to_owned())).unwrap())
        .await
        .unwrap();
    let (parts, body) = response.into_parts();
    let bytes = body.collect().await.unwrap().to_bytes();
    Reply {
        status: parts.status,
        headers: parts.headers,
        body: String::from_utf8_lossy(&bytes).into_owned(),
    }
}

async fn get(app: &Router, path: &str) -> Reply {
    send(app, Method::GET, path, &[("accept", "text/html")], "").await
}

/// A same-origin form post, as a browser sends it.
async fn post(app: &Router, path: &str, body: &str) -> Reply {
    send(
        app,
        Method::POST,
        path,
        &[
            ("content-type", "application/x-www-form-urlencoded"),
            ("sec-fetch-site", "same-origin"),
            ("accept", "text/html"),
        ],
        body,
    )
    .await
}

impl Rig {
    async fn queue(&self, work: u64) -> Job {
        match self
            .jobs
            .enqueue(
                WorkId::new(work).unwrap().into(),
                JobSource::Mail,
                self.clock.now(),
            )
            .await
            .unwrap()
        {
            EnqueueOutcome::Created(job) => job,
            EnqueueOutcome::AlreadyActive(id) => panic!("work {work} already active as {id}"),
        }
    }

    async fn titled(&self, work: u64, title: &str) {
        self.jobs
            .upsert(&BookRecord {
                work: WorkId::new(work).unwrap(),
                file_name: format!("{title} [{work}].epub"),
                title: title.to_owned(),
                updated_at: self.clock.now(),
            })
            .await
            .unwrap();
    }

    async fn claim(&self, id: JobId) -> Job {
        loop {
            let job = self
                .jobs
                .claim_next(self.clock.now())
                .await
                .unwrap()
                .expect("job due");
            if job.id == id {
                return job;
            }
            // Put an unrelated job straight back.
            let mut other = job;
            let at = other.updated_at;
            other.requeue_orphan(at).unwrap();
            self.jobs.save(&other).await.unwrap();
        }
    }

    async fn succeed(&self, id: JobId) {
        self.clock.advance(Duration::from_secs(1));
        let mut job = self.claim(id).await;
        job.succeed(self.clock.now()).unwrap();
        self.jobs.save(&job).await.unwrap();
    }

    async fn fail(&self, id: JobId, kind: FailureKind, message: &str) {
        self.clock.advance(Duration::from_secs(1));
        let mut job = self.claim(id).await;
        job.fail(self.clock.now(), kind, message.to_owned(), &POLICY, 0.5)
            .unwrap();
        self.jobs.save(&job).await.unwrap();
    }

    async fn log(&self, id: JobId, level: LogLevel, message: &str) {
        let entry = LogEntry {
            at: self.clock.now(),
            attempt: 0,
            level,
            message: message.to_owned(),
        };
        self.jobs.append_log(id, &entry).await.unwrap();
    }
}

// ----------------------------------------------------------------- dashboard

#[tokio::test]
async fn an_empty_dashboard_invites_the_first_action() {
    let rig = rig();
    let reply = get(&rig.ui, "/").await;
    assert_eq!(reply.status, StatusCode::OK);
    assert_eq!(
        reply.headers[header::CONTENT_TYPE],
        "text/html; charset=utf-8"
    );
    for text in [
        "Add a work",
        "Nothing needs attention.",
        "No downloads are running or waiting.",
        "Paste an AO3 link above",
        "AO3 reachable",
    ] {
        assert!(reply.body.contains(text), "missing {text:?}");
    }
}

#[tokio::test]
async fn the_page_is_usable_on_a_phone() {
    let rig = rig();
    let body = get(&rig.ui, "/").await.body;
    assert!(body.contains(r#"<meta name="viewport" content="width=device-width, initial-scale=1, viewport-fit=cover">"#));
    assert!(
        body.contains(r#"type="url""#) && body.contains(r#"inputmode="url""#),
        "URL keyboard on mobile"
    );
    assert!(body.contains(r#"autocapitalize="off""#));
    assert!(body.contains(r#"<html lang="en""#));
    let css = get(&rig.ui, "/static/app.css").await.body;
    for needle in [
        "min-height: 44px",
        "env(safe-area-inset-bottom)",
        "prefers-reduced-motion",
        "prefers-color-scheme: dark",
        "font-size: 1rem; /* 16px",
    ] {
        assert!(css.contains(needle), "css lacks {needle:?}");
    }
}

#[tokio::test]
async fn every_response_carries_the_hardening_headers() {
    let rig = rig();
    for path in ["/", "/jobs", "/static/app.css", "/nope"] {
        let reply = get(&rig.ui, path).await;
        let h = &reply.headers;
        assert!(
            h[header::CONTENT_SECURITY_POLICY]
                .to_str()
                .unwrap()
                .contains("default-src 'none'"),
            "{path}"
        );
        assert_eq!(h[header::X_CONTENT_TYPE_OPTIONS], "nosniff", "{path}");
        assert_eq!(h[header::REFERRER_POLICY], "no-referrer", "{path}");
        assert_eq!(h[header::X_FRAME_OPTIONS], "DENY", "{path}");
    }
    assert_eq!(
        get(&rig.ui, "/").await.headers[header::CACHE_CONTROL],
        "no-store"
    );
}

#[tokio::test]
async fn pages_contain_nothing_the_csp_would_block() {
    let rig = rig();
    let job = rig.queue(1).await;
    rig.titled(1, "A Story").await;
    rig.log(job.id, LogLevel::Info, "starting attempt 1").await;
    for path in ["/", "/jobs", &format!("/jobs/{}", job.id)] {
        let body = get(&rig.ui, path).await.body;
        assert!(!body.contains(" style="), "{path}: inline style");
        assert!(
            !body.contains("onclick=") && !body.contains("onerror=") && !body.contains("onload="),
            "{path}"
        );
        assert!(!body.contains("javascript:"), "{path}");
        assert_eq!(
            body.matches("<script").count(),
            1,
            "{path}: only the external script"
        );
        assert!(body.contains(r#"<script src="/static/app.js" defer></script>"#));
    }
}

#[tokio::test]
async fn the_dashboard_sorts_jobs_into_the_right_sections() {
    let rig = rig();
    let done = rig.queue(1).await;
    rig.titled(1, "Finished Tale").await;
    rig.succeed(done.id).await;
    let failed = rig.queue(2).await;
    rig.titled(2, "Gone Story").await;
    rig.fail(failed.id, FailureKind::NotFound, "deleted by the author")
        .await;
    let retrying = rig.queue(3).await;
    rig.titled(3, "Flaky Story").await;
    rig.fail(retrying.id, FailureKind::SiteUnavailable, "HTTP 525")
        .await;
    let _waiting = rig.queue(4).await;

    let body = get(&rig.ui, "/").await.body;
    let attention = body
        .split("Needs attention")
        .nth(1)
        .unwrap()
        .split("In progress")
        .next()
        .unwrap();
    assert!(
        attention.contains("Gone Story") && attention.contains("Failed"),
        "{attention}"
    );
    assert!(
        attention.contains("Flaky Story") && attention.contains("Retrying"),
        "{attention}"
    );
    assert!(attention.contains("deleted by the author") && attention.contains("HTTP 525"));
    assert!(!attention.contains("Finished Tale"));

    let progress = body
        .split("In progress")
        .nth(1)
        .unwrap()
        .split("Recent")
        .next()
        .unwrap();
    assert!(
        progress.contains("Work #4") && progress.contains("Queued"),
        "{progress}"
    );
    assert!(
        !progress.contains("Flaky Story"),
        "a retrying job belongs under attention"
    );

    let recent = body.split("Recent").last().unwrap();
    assert!(recent.contains("Finished Tale") && recent.contains("Done"));
    assert!(
        body.contains(&format!(r#"href="/jobs/{}""#, done.id)),
        "cards link to the job"
    );
}

#[tokio::test]
async fn the_shelf_shows_finished_jobs_oldest_first_with_accessible_labels() {
    let rig = rig();
    for n in 1..=3 {
        let job = rig.queue(n).await;
        rig.titled(n, &format!("Story {n}")).await;
        rig.succeed(job.id).await;
    }
    let body = get(&rig.ui, "/").await.body;
    let shelf = body
        .split(r#"class="shelf""#)
        .nth(1)
        .unwrap()
        .split("</ul>")
        .next()
        .unwrap();
    let order: Vec<usize> = (1..=3)
        .map(|n| shelf.find(&format!("Story {n}: Done")).unwrap())
        .collect();
    assert!(
        order.windows(2).all(|w| w[0] < w[1]),
        "oldest on the left: {order:?}"
    );
    assert!(shelf.contains("aria-label"));
}

#[tokio::test]
async fn the_site_pill_reflects_the_breaker() {
    let rig = rig();
    assert!(get(&rig.ui, "/").await.body.contains(r#"class="pill ok""#));
    *rig.site.0.lock().unwrap() = SiteStatus::Degraded {
        retry_at: fanfold_domain::timestamp::add(rig.clock.now(), Duration::from_secs(240)),
    };
    let body = get(&rig.ui, "/").await.body;
    assert!(
        body.contains(r#"class="pill bad""#)
            && body.contains("AO3 is having trouble. Trying again in 4 min"),
        "{body}"
    );
    *rig.site.0.lock().unwrap() = SiteStatus::Probing;
    assert!(get(&rig.ui, "/").await.body.contains("Checking AO3"));
}

#[tokio::test]
async fn fragments_are_just_the_live_region() {
    let rig = rig();
    rig.queue(1).await;
    let reply = send(&rig.ui, Method::GET, "/", &[("x-fragment", "1")], "").await;
    assert_eq!(reply.status, StatusCode::OK);
    assert!(!reply.body.contains("<html") && !reply.body.contains("<head"));
    assert!(reply.body.contains("pill") && reply.body.contains("Needs attention"));
    assert!(
        !reply.body.contains("Add a work"),
        "the form is outside the live region"
    );
}

// ----------------------------------------------------------------- hostile content

#[tokio::test]
async fn untrusted_text_is_escaped_everywhere() {
    let rig = rig();
    let job = rig.queue(1).await;
    let xss = "<script>alert(1)</script><img src=x onerror=alert(2)>";
    rig.titled(1, xss).await;
    rig.fail(job.id, FailureKind::Permanent, xss).await;
    rig.log(job.id, LogLevel::Error, xss).await;
    for path in [
        "/".to_owned(),
        "/jobs".to_owned(),
        format!("/jobs/{}", job.id),
    ] {
        let body = get(&rig.ui, &path).await.body;
        assert!(!body.contains("<script>alert"), "{path}");
        assert!(!body.contains("<img src=x"), "{path}");
        assert!(
            body.contains("&lt;script&gt;alert(1)&lt;/script&gt;"),
            "{path}: should be shown, escaped"
        );
    }
}

#[tokio::test]
async fn notices_never_reflect_arbitrary_text() {
    let rig = rig();
    let body = get(
        &rig.ui,
        "/?added=%3Cscript%3Ealert(1)%3C/script%3E&queued=%3Cb%3E",
    )
    .await
    .body;
    assert!(
        !body.contains("<script>alert") && !body.contains("notice"),
        "{body}"
    );
    assert!(
        get(&rig.ui, "/?added=7")
            .await
            .body
            .contains("Added to the queue.")
    );
}

// ----------------------------------------------------------------- history

#[tokio::test]
async fn history_filters_and_pages() {
    let rig = rig_with_page_size(2);
    for n in 1..=5 {
        let job = rig.queue(n).await;
        if n % 2 == 0 {
            rig.fail(job.id, FailureKind::Permanent, "gone").await;
        } else {
            rig.succeed(job.id).await;
        }
    }
    // Newest first: 5, 4 | 3, 2 | 1
    let page1 = get(&rig.ui, "/jobs").await.body;
    assert!(page1.contains("Work #5") && page1.contains("Work #4") && !page1.contains("Work #3"));
    assert!(page1.contains("Older →") && !page1.contains("← Newer"));
    let page3 = get(&rig.ui, "/jobs?page=3").await.body;
    assert!(page3.contains("Work #1") && page3.contains("← Newer") && !page3.contains("Older →"));

    let failed = get(&rig.ui, "/jobs?filter=failed").await.body;
    assert!(
        failed.contains("Work #4") && failed.contains("Work #2") && !failed.contains("Work #5")
    );
    assert!(failed.contains(r#"aria-current="page""#));

    // Junk parameters fall back to defaults instead of erroring.
    for query in [
        "filter=bogus",
        "page=0",
        "page=abc",
        "page=-3",
        "filter=&page=",
    ] {
        assert_eq!(
            get(&rig.ui, &format!("/jobs?{query}")).await.status,
            StatusCode::OK,
            "{query}"
        );
    }
    assert_eq!(get(&rig.ui, "/jobs?page=99").await.status, StatusCode::OK);
    assert!(
        get(&rig.ui, "/jobs?page=99")
            .await
            .body
            .contains("No jobs match this filter.")
    );
}

#[tokio::test]
async fn only_retryable_rows_get_a_checkbox_and_bulk_actions_appear() {
    let rig = rig();
    let ok = rig.queue(1).await;
    rig.succeed(ok.id).await;
    let bad = rig.queue(2).await;
    rig.fail(bad.id, FailureKind::Permanent, "x").await;

    let body = get(&rig.ui, "/jobs").await.body;
    assert_eq!(body.matches(r#"type="checkbox""#).count(), 1);
    assert!(body.contains(&format!(r#"value="{}""#, bad.id.get())));
    assert!(body.contains("Retry selected") && body.contains("Retry all 1 failed"));
    assert!(body.contains(r#"formaction="/jobs/retry-failed""#));

    let only_done = get(&rig.ui, "/jobs?filter=succeeded").await.body;
    assert_eq!(only_done.matches(r#"type="checkbox""#).count(), 0);
    assert!(
        only_done.contains("Retry all 1 failed"),
        "the global action stays available"
    );
}

// ----------------------------------------------------------------- job page

#[tokio::test]
async fn a_job_page_shows_its_story_and_the_log_by_run() {
    let rig = rig();
    let job = rig.queue(77).await;
    rig.titled(77, "The Long Fic").await;
    rig.log(job.id, LogLevel::Info, "starting attempt 1").await;
    rig.log(
        job.id,
        LogLevel::Error,
        "failed (site_unavailable): HTTP 525",
    )
    .await;
    rig.fail(job.id, FailureKind::SiteUnavailable, "HTTP 525")
        .await;
    rig.log(job.id, LogLevel::Info, "starting attempt 1").await;
    rig.log(job.id, LogLevel::Warn, "slow response\nsecond line")
        .await;

    let body = get(&rig.ui, &format!("/jobs/{}", job.id)).await.body;
    for text in [
        "The Long Fic",
        "Retrying",
        "https://archiveofourown.org/works/77",
        "Run 1",
        "Run 2",
        "failed (site_unavailable): HTTP 525",
        "slow response\nsecond line",
        "The Long Fic [77].epub",
        "0 of 5",
        "Next try",
    ] {
        assert!(body.contains(text), "missing {text:?}");
    }
    assert!(
        body.contains("lvl-error") && body.contains("ERROR") && body.contains("WARN"),
        "levels have labels, not just colours"
    );
    assert!(
        body.contains(r#"data-job=""#),
        "live updates are scoped to this job"
    );
    assert!(body.contains("rel=\"noopener noreferrer\""));
    let at = |text: &str| body.find(text).unwrap();
    assert!(at("Run 2") < at("Run 1"), "the latest run comes first");
    assert!(
        at("slow response") < at("failed (site_unavailable)") && at("Run 2") < at("slow response"),
        "each run keeps its own lines in order, under its heading"
    );
    assert!(body.contains("newest first"));
}

#[tokio::test]
async fn the_actions_offered_match_the_job_state() {
    let rig = rig();
    let queued = rig.queue(1).await;
    let failed = rig.queue(2).await;
    rig.fail(failed.id, FailureKind::Permanent, "x").await;
    let done = rig.queue(3).await;
    rig.succeed(done.id).await;

    let page = |id: JobId| {
        let ui = rig.ui.clone();
        async move { get(&ui, &format!("/jobs/{id}")).await.body }
    };
    let q = page(queued.id).await;
    assert!(
        q.contains(&format!(r#"action="/jobs/{}/cancel""#, queued.id)) && !q.contains("/retry\"")
    );
    let f = page(failed.id).await;
    assert!(
        f.contains(&format!(r#"action="/jobs/{}/retry""#, failed.id)) && !f.contains("/cancel\"")
    );
    let d = page(done.id).await;
    assert!(!d.contains("/retry\"") && !d.contains("/cancel\""));
    let force = |id: JobId| format!(r#"action="/jobs/{id}/force-retry""#);
    assert!(
        d.contains(&force(done.id)),
        "a finished book can be re-downloaded"
    );
    assert!(f.contains(&force(failed.id)));
    assert!(!q.contains("force-retry"), "not while it is queued");
}

#[tokio::test]
async fn a_finished_job_can_be_downloaded_again_from_scratch() {
    let rig = rig();
    let done = rig.queue(1).await;
    rig.succeed(done.id).await;
    let reply = post(&rig.ui, &format!("/jobs/{}/force-retry", done.id), "").await;
    assert_eq!(
        (reply.status, reply.location()),
        (
            StatusCode::SEE_OTHER,
            format!("/jobs/{0}?forced={0}", done.id).as_str()
        )
    );
    let stored = JobRepository::get(rig.jobs.as_ref(), done.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        (stored.state, stored.force),
        (fanfold_domain::job::JobState::Queued, true)
    );
    let page = get(&rig.ui, reply.location()).await.body;
    assert!(page.contains("Queued to download again from scratch."));
    assert!(page.contains("from scratch"));

    let again = post(&rig.ui, &format!("/jobs/{}/force-retry", done.id), "").await;
    assert_eq!(again.status, StatusCode::CONFLICT, "already queued");
    assert_eq!(
        post(&rig.ui, "/jobs/999/force-retry", "").await.status,
        StatusCode::NOT_FOUND
    );
}

#[tokio::test]
async fn unknown_jobs_are_404_problems_in_the_format_the_client_asked_for() {
    let rig = rig();
    for path in [
        "/jobs/999",
        "/jobs/abc",
        "/jobs/0",
        "/jobs/-1",
        "/jobs/1.5",
        "/jobs/99999999999999999999",
    ] {
        let json = send(&rig.ui, Method::GET, path, &[], "").await;
        assert_eq!(json.status, StatusCode::NOT_FOUND, "{path}");
        assert_eq!(
            json.headers[header::CONTENT_TYPE],
            "application/problem+json"
        );
        let doc: serde_json::Value = serde_json::from_str(&json.body).unwrap();
        assert_eq!(
            (doc["status"].clone(), doc["type"].as_str().is_some()),
            (404.into(), true)
        );

        let html = get(&rig.ui, path).await;
        assert_eq!(html.status, StatusCode::NOT_FOUND, "{path}");
        assert!(
            html.headers[header::CONTENT_TYPE]
                .to_str()
                .unwrap()
                .starts_with("text/html")
        );
        assert!(html.body.contains("Back to the dashboard"));
    }
    assert_eq!(
        send(&rig.ui, Method::GET, "/nope", &[], "").await.status,
        StatusCode::NOT_FOUND
    );
}

// ----------------------------------------------------------------- actions

#[tokio::test]
async fn adding_a_link_queues_it_and_redirects_back() {
    let rig = rig();
    let reply = post(
        &rig.ui,
        "/jobs",
        "url=https%3A%2F%2Farchiveofourown.org%2Fworks%2F123%3Fview_adult%3Dtrue",
    )
    .await;
    assert_eq!(reply.status, StatusCode::SEE_OTHER);
    assert_eq!(reply.location(), "/?added=1");
    assert_eq!(rig.jobs.all_jobs().len(), 1);
    assert_eq!(
        rig.jobs.all_jobs()[0].target,
        WorkId::new(123).unwrap().into()
    );

    let again = post(
        &rig.ui,
        "/jobs",
        "url=https%3A%2F%2Farchiveofourown.org%2Fworks%2F123",
    )
    .await;
    assert_eq!(
        (again.status, again.location()),
        (StatusCode::SEE_OTHER, "/?queued=1")
    );
    assert_eq!(rig.jobs.all_jobs().len(), 1, "no duplicate");
    assert!(
        get(&rig.ui, "/?queued=1")
            .await
            .body
            .contains("already queued")
    );
}

#[tokio::test]
async fn a_bad_link_explains_what_to_do() {
    let rig = rig();
    let html = post(
        &rig.ui,
        "/jobs",
        "url=https%3A%2F%2Fexample.com%2Fworks%2F1",
    )
    .await;
    assert_eq!(html.status, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(html.body.contains("Not an AO3 work URL"), "{}", html.body);
    assert!(html.body.contains("link to an Archive of Our Own work"));

    let json = send(
        &rig.ui,
        Method::POST,
        "/jobs",
        &[
            ("content-type", "application/x-www-form-urlencoded"),
            ("sec-fetch-site", "same-origin"),
        ],
        "url=nonsense",
    )
    .await;
    assert_eq!(json.status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(
        json.headers[header::CONTENT_TYPE],
        "application/problem+json"
    );
    let doc: serde_json::Value = serde_json::from_str(&json.body).unwrap();
    assert_eq!(doc["errors"][0]["pointer"], "#/url");

    assert_eq!(
        post(&rig.ui, "/jobs", "").await.status,
        StatusCode::UNPROCESSABLE_ENTITY,
        "no url at all"
    );
    assert_eq!(rig.jobs.all_jobs(), Vec::new());
}

#[tokio::test]
async fn cross_site_posts_are_refused_before_anything_happens() {
    let rig = rig();
    let body = "url=https%3A%2F%2Farchiveofourown.org%2Fworks%2F1";
    let form = ("content-type", "application/x-www-form-urlencoded");
    for headers in [
        vec![form, ("sec-fetch-site", "cross-site")],
        vec![form, ("sec-fetch-site", "same-site")],
        vec![
            form,
            ("origin", "https://evil.example"),
            ("host", "fanfold.example"),
        ],
        vec![form, ("origin", "null"), ("host", "fanfold.example")],
    ] {
        let reply = send(&rig.ui, Method::POST, "/jobs", &headers, body).await;
        assert_eq!(reply.status, StatusCode::FORBIDDEN, "{headers:?}");
        assert!(reply.body.contains("Cross-site requests are not allowed."));
    }
    assert_eq!(rig.jobs.all_jobs(), Vec::new());
    // The same request from a non-browser client (no fetch metadata) is fine.
    let ok = send(&rig.ui, Method::POST, "/jobs", &[form], body).await;
    assert_eq!(ok.status, StatusCode::SEE_OTHER);
}

#[tokio::test]
async fn oversized_bodies_are_rejected() {
    let rig = rig();
    let big = format!("url={}", "a".repeat(100_000));
    assert_eq!(
        post(&rig.ui, "/jobs", &big).await.status,
        StatusCode::PAYLOAD_TOO_LARGE
    );
}

#[tokio::test]
async fn retry_and_cancel_from_the_job_page() {
    let rig = rig();
    let failed = rig.queue(1).await;
    rig.fail(failed.id, FailureKind::Permanent, "gone").await;
    let reply = post(&rig.ui, &format!("/jobs/{}/retry", failed.id), "").await;
    assert_eq!(
        (reply.status, reply.location()),
        (
            StatusCode::SEE_OTHER,
            format!("/jobs/{0}?retried_one={0}", failed.id).as_str()
        )
    );
    let stored = JobRepository::get(rig.jobs.as_ref(), failed.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        (stored.state, stored.attempt),
        (fanfold_domain::job::JobState::Queued, 0)
    );

    // Now queued: it can be cancelled but not retried again.
    let wrong = post(&rig.ui, &format!("/jobs/{}/retry", failed.id), "").await;
    assert_eq!(wrong.status, StatusCode::CONFLICT);
    assert!(
        wrong.body.contains("cannot retry a job that is queued"),
        "{}",
        wrong.body
    );
    let cancelled = post(&rig.ui, &format!("/jobs/{}/cancel", failed.id), "").await;
    assert_eq!(cancelled.status, StatusCode::SEE_OTHER);
    assert_eq!(
        JobRepository::get(rig.jobs.as_ref(), failed.id)
            .await
            .unwrap()
            .unwrap()
            .state,
        fanfold_domain::job::JobState::Cancelled
    );

    assert_eq!(
        post(&rig.ui, "/jobs/999/retry", "").await.status,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        post(&rig.ui, "/jobs/abc/cancel", "").await.status,
        StatusCode::NOT_FOUND
    );
}

#[tokio::test]
async fn retrying_a_work_that_was_queued_again_is_a_clear_conflict() {
    let rig = rig();
    let old = rig.queue(1).await;
    rig.fail(old.id, FailureKind::Permanent, "x").await;
    rig.queue(1).await; // an email queued it again
    let reply = post(&rig.ui, &format!("/jobs/{}/retry", old.id), "").await;
    assert_eq!(reply.status, StatusCode::CONFLICT);
    assert!(reply.body.contains("Already queued"), "{}", reply.body);
}

#[tokio::test]
async fn bulk_retry_reports_what_it_did() {
    let rig = rig();
    let mut failed = Vec::new();
    for n in 1..=3 {
        let job = rig.queue(n).await;
        rig.fail(job.id, FailureKind::Permanent, "x").await;
        failed.push(job.id);
    }
    rig.queue(2).await; // work 2 is already queued again, so retrying its old job conflicts

    let none = post(&rig.ui, "/jobs/retry", "other=1").await;
    assert_eq!(none.status, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(none.body.contains("Select at least one job"));

    let body = failed
        .iter()
        .map(|id| format!("id={}", id.get()))
        .collect::<Vec<_>>()
        .join("&");
    let reply = post(&rig.ui, "/jobs/retry", &body).await;
    assert_eq!(
        (reply.status, reply.location()),
        (
            StatusCode::SEE_OTHER,
            "/jobs?filter=failed&retried=2&skipped=1"
        )
    );
    let notice = get(&rig.ui, reply.location()).await.body;
    assert!(
        notice.contains("Queued 2 again.") && notice.contains("Skipped 1"),
        "{notice}"
    );
}

#[tokio::test]
async fn retry_all_failed() {
    let rig = rig();
    for n in 1..=3 {
        let job = rig.queue(n).await;
        rig.fail(job.id, FailureKind::Permanent, "x").await;
    }
    let reply = post(&rig.ui, "/jobs/retry-failed", "").await;
    assert_eq!(reply.location(), "/jobs?filter=failed&retried=3&skipped=0");
    assert_eq!(rig.jobs.counts().await.unwrap().failed, 0);
}

#[tokio::test]
async fn storage_outages_are_503_problems_that_do_not_leak() {
    let rig = rig();
    rig.jobs
        .break_with(Some(fanfold_app::error::RepoError::Backend(
            "password=hunter2 host=10.1.2.3".into(),
        )));
    for reply in [
        get(&rig.ui, "/").await,
        get(&rig.ui, "/jobs").await,
        post(
            &rig.ui,
            "/jobs",
            "url=https%3A%2F%2Farchiveofourown.org%2Fworks%2F1",
        )
        .await,
    ] {
        assert_eq!(reply.status, StatusCode::SERVICE_UNAVAILABLE);
        assert!(
            !reply.body.contains("hunter2") && !reply.body.contains("10.1.2.3"),
            "{}",
            reply.body
        );
        assert_eq!(reply.headers[header::RETRY_AFTER], "5");
    }
}

// ----------------------------------------------------------------- live updates

#[tokio::test]
async fn the_event_stream_announces_changes() {
    let rig = rig();
    let response = rig
        .ui
        .clone()
        .oneshot(
            Request::builder()
                .uri("/events")
                .header("accept", "text/event-stream")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.headers()[header::CONTENT_TYPE],
        "text/event-stream"
    );
    assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");

    let job = rig.queue(5).await;
    rig.log(job.id, LogLevel::Info, "hello").await;

    let mut body = response.into_body();
    let mut seen = String::new();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while !(seen.contains(&format!("data: job {}", job.id))
        && seen.contains(&format!("data: log {}", job.id)))
    {
        let frame = tokio::time::timeout_at(deadline, body.frame())
            .await
            .expect("events arrive")
            .expect("stream open")
            .unwrap();
        if let Ok(data) = frame.into_data() {
            seen.push_str(&String::from_utf8_lossy(&data));
        }
    }
    assert!(seen.contains("event: change"), "{seen}");
}

#[tokio::test]
async fn a_slow_reader_is_told_to_resync() {
    let rig = rig();
    let response = rig
        .ui
        .clone()
        .oneshot(
            Request::builder()
                .uri("/events")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    // Overflow the channel (256) without reading, so the reader lags.
    for n in 1..=300 {
        rig.queue(n).await;
    }
    let mut body = response.into_body();
    let frame = tokio::time::timeout(Duration::from_secs(5), body.frame())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    let text = String::from_utf8_lossy(&frame.into_data().unwrap()).into_owned();
    assert!(text.contains("data: resync"), "{text}");
}

// ----------------------------------------------------------------- assets & ops

#[tokio::test]
async fn static_assets_are_served_with_the_right_types() {
    let rig = rig();
    for (path, ctype) in [
        ("/static/app.css", "text/css; charset=utf-8"),
        ("/static/app.js", "text/javascript; charset=utf-8"),
        ("/static/icon.svg", "image/svg+xml"),
    ] {
        let reply = get(&rig.ui, path).await;
        assert_eq!(reply.status, StatusCode::OK, "{path}");
        assert_eq!(reply.headers[header::CONTENT_TYPE], ctype, "{path}");
        assert!(
            reply.headers[header::CACHE_CONTROL]
                .to_str()
                .unwrap()
                .starts_with("public")
        );
        assert_ne!(reply.body, "");
    }
}

#[tokio::test]
async fn ops_endpoints() {
    let rig = rig();
    rig.queue(1).await;
    assert_eq!(get(&rig.ops, "/healthz").await.body, "ok");
    assert_eq!(get(&rig.ops, "/readyz").await.status, StatusCode::OK);

    let metrics = get(&rig.ops, "/metrics").await;
    assert_eq!(metrics.status, StatusCode::OK);
    assert!(
        metrics.headers[header::CONTENT_TYPE]
            .to_str()
            .unwrap()
            .starts_with("text/plain; version=0.0.4")
    );
    assert!(
        metrics.body.contains("fanfold_jobs{state=\"queued\"} 1"),
        "{}",
        metrics.body
    );
    assert!(
        metrics
            .body
            .contains("fanfold_build_info{version=\"test\"} 1")
    );

    rig.jobs
        .break_with(Some(fanfold_app::error::RepoError::Backend("down".into())));
    let not_ready = send(&rig.ops, Method::GET, "/readyz", &[], "").await;
    assert_eq!(not_ready.status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(
        not_ready.headers[header::CONTENT_TYPE],
        "application/problem+json"
    );
    assert_eq!(
        get(&rig.ops, "/healthz").await.status,
        StatusCode::OK,
        "liveness does not depend on the database"
    );
}

#[tokio::test]
async fn metrics_and_probes_are_not_exposed_through_the_user_interface() {
    let rig = rig();
    for path in ["/metrics", "/healthz", "/readyz"] {
        assert_eq!(
            get(&rig.ui, path).await.status,
            StatusCode::NOT_FOUND,
            "{path}"
        );
    }
    for path in ["/", "/jobs", "/events"] {
        assert_eq!(
            get(&rig.ops, path).await.status,
            StatusCode::NOT_FOUND,
            "{path}"
        );
    }
}

#[tokio::test]
async fn open_event_streams_end_when_the_server_shuts_down() {
    let rig = rig();
    let response = rig
        .ui
        .clone()
        .oneshot(
            Request::builder()
                .uri("/events")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let mut body = response.into_body();
    rig.stop.send(true).unwrap();
    let ended = tokio::time::timeout(Duration::from_secs(5), async {
        while let Some(frame) = body.frame().await {
            frame.unwrap();
        }
    })
    .await;
    assert!(
        ended.is_ok(),
        "the stream must end so a graceful shutdown can finish"
    );
}

#[tokio::test]
async fn the_pill_comes_first_and_updates_travel_with_a_marker() {
    let rig = rig();
    let page = get(&rig.ui, "/").await.body;
    let (pill, form, live) = (
        page.find("data-pill").unwrap(),
        page.find("Add a work").unwrap(),
        page.find(r#"id="live""#).unwrap(),
    );
    assert!(
        pill < form && form < live,
        "status first, then the form, then the live content"
    );
    assert_eq!(
        page.matches("data-pill").count(),
        1,
        "the full page has exactly one pill"
    );
    let fragment = send(&rig.ui, Method::GET, "/", &[("x-fragment", "1")], "")
        .await
        .body;
    assert!(
        fragment.starts_with("<div id=\"site\" data-pill"),
        "the script moves this into place"
    );
}

#[tokio::test]
async fn untitled_works_do_not_repeat_their_number() {
    let rig = rig();
    rig.queue(2002).await;
    rig.titled(2003, "A Titled One").await;
    let titled = rig.queue(2003).await;
    let body = get(&rig.ui, "/").await.body;
    assert_eq!(
        body.matches("2002").count(),
        1,
        "shown once, as the name, not repeated beside it: {body}"
    );
    assert!(
        body.contains("#2003"),
        "a titled work still shows its number"
    );
    let detail = get(&rig.ui, &format!("/jobs/{}", titled.id)).await.body;
    assert!(detail.contains("work #2003"));
}

#[tokio::test]
async fn failures_listed_under_attention_are_not_repeated_under_recent() {
    let rig = rig();
    let failed = rig.queue(1).await;
    rig.titled(1, "Gone Story").await;
    rig.fail(failed.id, FailureKind::NotFound, "deleted").await;
    let ok = rig.queue(2).await;
    rig.titled(2, "Fine Story").await;
    rig.succeed(ok.id).await;
    let body = get(&rig.ui, "/").await.body;
    assert_eq!(
        body.matches("Gone Story").count(),
        3,
        "the card title, plus the shelf spine's title and aria-label"
    );
    let recent = body.split("Recent").last().unwrap();
    assert!(
        recent.contains("Fine Story") && !recent.contains("Gone Story"),
        "{recent}"
    );
}

// ---------------------------------------------------- gaps found by mutation testing

#[tokio::test]
async fn every_page_has_working_navigation_and_relative_times() {
    let rig = rig();
    let job = rig.queue(1).await;
    for (path, current) in [("/", "Dashboard"), ("/jobs", "History")] {
        let body = get(&rig.ui, path).await.body;
        assert!(
            body.contains(r#"<a href="/">Dashboard</a>"#)
                || body.contains(r#"href="/" aria-current="page">Dashboard"#),
            "{path}"
        );
        assert!(
            body.contains(r#"href="/jobs""#) && body.contains("History"),
            "{path}"
        );
        assert!(
            body.contains(&format!(r#"aria-current="page">{current}</a>"#)),
            "{path}: marks where you are"
        );
        assert!(
            body.contains(r#"<time datetime=""#),
            "{path}: times are real <time> elements"
        );
    }
    let detail = get(&rig.ui, &format!("/jobs/{}", job.id)).await.body;
    assert!(detail.contains(r#"<time datetime=""#));
    assert!(detail.contains(r#"href="/""#) && detail.contains(r#"href="/jobs""#));
}

#[tokio::test]
async fn history_and_job_pages_refresh_as_fragments_too() {
    let rig = rig();
    let job = rig.queue(5).await;
    rig.titled(5, "Live Story").await;
    for path in ["/jobs".to_owned(), format!("/jobs/{}", job.id)] {
        let fragment = send(&rig.ui, Method::GET, &path, &[("x-fragment", "1")], "").await;
        assert_eq!(fragment.status, StatusCode::OK, "{path}");
        assert!(!fragment.body.contains("<html"), "{path}: no page chrome");
        assert!(
            fragment.body.starts_with("<div id=\"site\" data-pill"),
            "{path}: the pill travels with it"
        );
        assert!(
            fragment.body.contains("Live Story"),
            "{path}: and the content: {}",
            fragment.body
        );
        assert_eq!(fragment.headers[header::VARY], "x-fragment");
    }
}

#[tokio::test]
async fn filter_chips_show_the_right_counts() {
    let rig = rig();
    // 2 queued, 3 running, 4 retrying, 1 failed, 5 done: every count distinct.
    for n in 1..=2 {
        rig.queue(n).await;
    }
    for n in 3..=5 {
        let job = rig.queue(n).await;
        rig.clock.advance(Duration::from_secs(1));
        rig.claim(job.id).await;
    }
    for n in 6..=9 {
        let job = rig.queue(n).await;
        rig.fail(job.id, FailureKind::SiteUnavailable, "503").await;
    }
    let failed = rig.queue(10).await;
    rig.fail(failed.id, FailureKind::Permanent, "gone").await;
    for n in 11..=15 {
        let job = rig.queue(n).await;
        rig.succeed(job.id).await;
    }
    let body = get(&rig.ui, "/jobs").await.body;
    for (label, count) in [("Active", 9), ("Retrying", 4), ("Failed", 1), ("Done", 5)] {
        let chip = format!(r#"{label} <span class="mono">{count}</span>"#);
        assert!(body.contains(&chip), "expected {chip:?} in the filters");
    }
}

#[tokio::test]
async fn the_source_of_a_job_is_named() {
    let rig = rig();
    let mail = rig.queue(1).await; // queued by an email
    post(
        &rig.ui,
        "/jobs",
        "url=https%3A%2F%2Farchiveofourown.org%2Fworks%2F2",
    )
    .await;
    assert!(
        get(&rig.ui, &format!("/jobs/{}", mail.id))
            .await
            .body
            .contains("AO3 email")
    );
    assert!(get(&rig.ui, "/jobs/2").await.body.contains("Added by hand"));
}

#[tokio::test]
async fn the_last_page_is_recognised_even_when_it_is_exactly_full() {
    let rig = rig_with_page_size(2);
    for n in 1..=4 {
        let job = rig.queue(n).await;
        rig.succeed(job.id).await;
    }
    let second = get(&rig.ui, "/jobs?page=2").await.body;
    assert!(
        second.contains("← Newer") && !second.contains("Older →"),
        "4 jobs at 2 per page end on page 2"
    );
    assert!(get(&rig.ui, "/jobs").await.body.contains("Older →"));
}

#[tokio::test]
async fn job_id_zero_is_rejected_as_malformed_not_merely_missing() {
    let rig = rig();
    let body = send(&rig.ui, Method::GET, "/jobs/0", &[], "").await.body;
    assert!(body.contains("not a valid job id"), "{body}");
    let missing = send(&rig.ui, Method::GET, "/jobs/12345", &[], "")
        .await
        .body;
    assert!(missing.contains("No job with id 12345"), "{missing}");
}

#[tokio::test]
async fn ordinary_sized_bodies_are_accepted_and_only_huge_ones_refused() {
    let rig = rig();
    // 5 KB of padding after a valid URL: well within 64 KiB.
    let body = format!(
        "url=https%3A%2F%2Farchiveofourown.org%2Fworks%2F1&note={}",
        "a".repeat(5_000)
    );
    assert_eq!(
        post(&rig.ui, "/jobs", &body).await.status,
        StatusCode::SEE_OTHER
    );
    let huge = format!("url=x&note={}", "a".repeat(70_000));
    assert_eq!(
        post(&rig.ui, "/jobs", &huge).await.status,
        StatusCode::PAYLOAD_TOO_LARGE
    );
}

#[tokio::test]
async fn apostrophes_cannot_break_out_of_attributes() {
    // maud escapes < > & " but not ', which is safe only because every attribute value
    // it writes is double-quoted. Pin that: an apostrophe payload must stay inside
    // the attribute it was written into.
    let rig = rig();
    let job = rig.queue(1).await;
    let payload = "x' onmouseover='alert(1)";
    rig.titled(1, payload).await;
    rig.succeed(job.id).await;

    let body = get(&rig.ui, "/").await.body;
    let attr = format!(r#"title="{payload}: Done""#);
    assert!(
        body.contains(&attr),
        "the whole payload stays inside one double-quoted attribute"
    );
    // Apart from the payload's own text, no attribute in the page is single-quoted.
    let markup_only = body.replace(payload, "");
    assert!(
        !markup_only.contains("='"),
        "attributes are double-quoted, so ' cannot end one"
    );
}

// ------------------------------------------------------- phone and desktop layouts

#[tokio::test]
async fn the_stylesheet_has_a_wide_screen_layout_as_well_as_the_phone_one() {
    let rig = rig();
    let css = get(&rig.ui, "/static/app.css").await.body;
    let wide = css
        .split("@media (min-width: 900px)")
        .nth(1)
        .expect("a desktop section");
    for needle in [
        ".board { display: grid",             // work queue beside the history
        "grid-template-columns: var(--cols)", // history as a table
        ".jobgrid { display: grid",           // facts beside the log
        "position: sticky",                   // the facts stay put while the log scrolls
        ".bar { flex-direction: row-reverse", // form and status share a row
    ] {
        assert!(wide.contains(needle), "desktop rules lack {needle:?}");
    }
    assert!(
        css.contains("@media (min-width: 1280px)"),
        "and room to breathe on large screens"
    );
    // The phone rules still come first and stand alone.
    assert!(
        css.find(".cells { display: flex").unwrap()
            < css.find("@media (min-width: 900px)").unwrap()
    );
}

#[tokio::test]
async fn the_dashboard_markup_lets_the_queue_sit_beside_the_history() {
    let rig = rig();
    let job = rig.queue(1).await;
    rig.fail(job.id, FailureKind::Permanent, "gone").await;
    rig.queue(2).await;
    let done = rig.queue(3).await;
    rig.succeed(done.id).await;
    let body = get(&rig.ui, "/").await.body;
    let main = body.find(r#"class="board-main""#).expect("main column");
    let side = body.find(r#"class="board-side""#).expect("side column");
    let (attention, progress, recent) = (
        body.find("Needs attention").unwrap(),
        body.find("In progress").unwrap(),
        body.rfind("Recent").unwrap(),
    );
    assert!(
        main < attention && attention < progress && progress < side && side < recent,
        "attention and in-progress are the main column; recent is the side column"
    );
}

#[tokio::test]
async fn history_rows_are_cells_that_can_line_up_as_a_table() {
    let rig = rig();
    let ok = rig.queue(1).await;
    rig.succeed(ok.id).await;
    let bad = rig.queue(2).await;
    rig.fail(bad.id, FailureKind::Permanent, "gone").await;
    rig.queue(3).await;
    let body = get(&rig.ui, "/jobs").await.body;

    assert!(
        body.contains(r#"class="thead" aria-hidden="true""#),
        "column headers, hidden from screen readers"
    );
    for label in ["Status", "Work", "ID", "Tries", "Updated", "Last error"] {
        assert!(
            body.contains(&format!("<span>{label}</span>")),
            "header {label}"
        );
    }
    assert_eq!(
        body.matches(r#"class="body cells""#).count(),
        3,
        "every row uses the same cells"
    );
    assert_eq!(body.matches(r#"class="c-status""#).count(), 3);
    assert_eq!(body.matches(r#"class="c-title""#).count(), 3);
    // Rows that cannot be selected keep the column aligned with a spacer. It must not
    // reuse the empty-state class, which draws a dashed box.
    assert_eq!(body.matches("pick-spacer").count(), 2);
    assert!(!body.contains("pick empty"));
}

#[tokio::test]
async fn the_job_page_markup_puts_the_facts_beside_the_log() {
    let rig = rig();
    let job = rig.queue(1).await;
    rig.log(job.id, LogLevel::Info, "starting attempt 1").await;
    let body = get(&rig.ui, &format!("/jobs/{}", job.id)).await.body;
    let (grid, side, log) = (
        body.find(r#"class="jobgrid""#).unwrap(),
        body.find(r#"class="jobside""#).unwrap(),
        body.find(r#"role="log""#).unwrap(),
    );
    assert!(grid < side && side < log);
    assert!(
        body.find("Open on AO3").unwrap() < log,
        "facts come before the log in reading order too"
    );
}

// ------------------------------------------------------------------- forms

fn form(fields: &[(&str, &str)]) -> String {
    url::form_urlencoded::Serializer::new(String::new())
        .extend_pairs(fields)
        .finish()
}

// --------------------------------------------------------------------- library

impl Rig {
    /// A downloaded book whose work AO3 described (the testkit's realistic observation, edited).
    async fn observed(
        &self,
        work: u64,
        edit: impl FnOnce(&mut fanfold_domain::metadata::WorkMetadata),
    ) {
        let mut meta = fanfold_testkit::contracts::observed_work(work);
        edit(&mut meta);
        fanfold_app::ports::MetadataStore::record_work(
            self.jobs.as_ref(),
            &meta,
            self.clock.now(),
            None,
        )
        .await
        .unwrap();
        self.titled(work, &meta.facts.title).await;
    }
}

#[tokio::test]
async fn the_library_lists_every_book_with_what_ao3_said() {
    let rig = rig();
    let empty = get(&rig.ui, "/library").await;
    assert_eq!(empty.status, StatusCode::OK);
    assert!(empty.body.contains("Your library is empty"));
    assert!(
        empty
            .body
            .contains(r#"href="/library" aria-current="page""#),
        "Library is in the navigation, and marked as the current page"
    );

    rig.observed(1, |m| {
        m.facts.title = "Alpha Light".into();
        m.facts.chapters_total = Some(2);
    })
    .await;
    rig.observed(2, |m| m.facts.title = "Beta Tide".into())
        .await;
    rig.titled(3, "Gamma").await;

    let page = get(&rig.ui, "/library").await.body;
    for needle in [
        "Alpha Light",
        "Beta Tide",
        "Gamma",
        "<strong>3</strong> books",
        "<strong>1</strong> complete",
        "Original Work",
        "by quillandink",
        "Not checked yet",
        r#"href="/library/1""#,
        r#"class="rating r-t""#,
        "Part 2 of",
        "The Harbor Saga",
        "120k words",
    ] {
        assert!(page.contains(needle), "{needle:?} in the library");
    }

    let complete = get(&rig.ui, "/library?status=complete").await.body;
    assert!(complete.contains("Alpha Light") && !complete.contains("Beta Tide"));
    assert!(complete.contains("1 of 3 books"));
    assert!(
        complete.contains(r#"value="is:complete""#),
        "an old link's filter becomes part of the search, so searching keeps it"
    );

    let search = get(&rig.ui, "/library?q=beta").await.body;
    assert!(search.contains("Beta Tide") && !search.contains("Alpha Light"));
    assert!(search.contains("books match “beta”"));
    assert!(
        search.contains(r#"value="beta""#),
        "the box keeps the search"
    );
    assert!(
        get(&rig.ui, "/library?q=zzz")
            .await
            .body
            .contains("No books match"),
        "a search that finds nothing says so"
    );
    assert_eq!(
        get(&rig.ui, "/library?status=nope&sort=nope&page=0")
            .await
            .status,
        StatusCode::OK,
        "unknown parameters fall back to the defaults"
    );

    let fragment = send(&rig.ui, Method::GET, "/library", &[("x-fragment", "1")], "").await;
    assert!(!fragment.body.contains("<html") && fragment.body.contains("Alpha Light"));
    assert!(
        fragment.body.contains("data-pill"),
        "a live update carries the site pill"
    );
}

#[tokio::test]
async fn the_library_pages_through_many_books() {
    let rig = rig_with_page_size(2);
    for n in 1..=5 {
        rig.titled(n, &format!("Book {n}")).await;
    }
    let first = get(&rig.ui, "/library?sort=title").await.body;
    assert!(first.contains("Book 1") && first.contains("Book 2") && !first.contains("Book 3"));
    assert!(first.contains(r#"href="/library?sort=title&amp;page=2""#));
    assert!(!first.contains(r#"rel="prev""#));
    let last = get(&rig.ui, "/library?sort=title&page=3").await.body;
    assert!(last.contains("Book 5") && !last.contains("Book 4"));
    assert!(last.contains(r#"rel="prev""#) && !last.contains(r#"rel="next""#));
}

#[tokio::test]
async fn a_book_page_shows_everything_and_never_trusts_the_summary() {
    let rig = rig();
    rig.observed(7, |m| {
        m.facts.summary_html =
            r#"<p>Calm <script>alert("x")</script>&amp; quiet</p><img src=x onerror=boom>"#.into();
    })
    .await;
    let page = get(&rig.ui, "/library/7").await;
    assert_eq!(page.status, StatusCode::OK);
    for needle in [
        "The Keeper and the Logbook",
        "by quillandink",
        "Ada Marsh/Ben Ward",
        "Enemies to Lovers",
        "1. First Light",
        "120,000",
        "1,234",
        r#"href="https://archiveofourown.org/works/7""#,
        r#"<form method="post" action="/jobs">"#,
        r#"value="https://archiveofourown.org/works/7""#,
        "Delete the book",
        "The Keeper and the Logbook [7].epub",
        r#"aria-current="page">Library"#,
    ] {
        assert!(page.body.contains(needle), "{needle:?} on the book page");
    }
    assert!(
        !page.body.contains("<script>alert") && !page.body.contains("<img src=x"),
        "the summary's markup never reaches the page"
    );
    assert!(
        page.body.contains("<p>Calm alert"),
        "its text does, escaped"
    );

    assert_eq!(
        get(&rig.ui, "/library/999").await.status,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        get(&rig.ui, "/library/abc").await.status,
        StatusCode::NOT_FOUND
    );
}

#[tokio::test]
async fn deleting_from_a_book_page_returns_to_the_library() {
    let rig = rig();
    rig.observed(5, |_| {}).await;
    rig.library.seed("The Keeper and the Logbook [5].epub");
    let page = get(&rig.ui, "/library/5").await.body;
    assert!(page.contains(r#"name="back" value="library""#));

    let reply = post(&rig.ui, "/works/5/delete", &form(&[("back", "library")])).await;
    assert_eq!(reply.status, StatusCode::SEE_OTHER);
    assert_eq!(reply.location(), "/library?deleted_book=1");
    assert_eq!(rig.library.files(), Vec::<String>::new());
    let after = get(&rig.ui, reply.location()).await.body;
    assert!(after.contains("The book was deleted from the library"));
    assert!(after.contains("Your library is empty"));
}
