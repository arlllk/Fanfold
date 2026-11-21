//! Routes and handlers.
//!
//! Two routers: the **UI** (pages, actions, live events), which sits behind the
//! gateway's login, and **ops** (`/healthz`, `/readyz`, `/metrics`), which Kubernetes
//! and Prometheus reach directly on a separate port.
//!
//! Pages are rendered completely on the server. Live updates re-request the same URL
//! with `X-Fragment: 1` and swap in the returned body, so there is one rendering path.

// Problem documents are only built on error paths, so the size of an `Err` is
// irrelevant to throughput; boxing them would only add noise to every handler.
#![allow(clippy::result_large_err)]

use std::{collections::HashMap, convert::Infallible, sync::Arc, time::Duration};

use axum::{
    Router,
    body::Bytes,
    extract::{DefaultBodyLimit, Path, RawQuery, State},
    http::{HeaderMap, HeaderValue, StatusCode, header},
    middleware,
    response::{
        IntoResponse, Redirect, Response,
        sse::{Event as SseEvent, KeepAlive, Sse},
    },
    routing::{get, post},
};
use fanfold_app::{
    admin::{AdminError, JobAdmin},
    ports::{
        BookCatalog, Clock, Event, EventSource, JobFilter, JobRepository, LibraryCatalog,
        LibraryQuery, LibrarySort, LibraryStatus, MetadataStore, Readiness, SiteStatusSource,
    },
    stats::{MailStats, WorkerStats},
};
use fanfold_domain::{
    job::{Job, JobId, Target},
    work::{SeriesId, WorkId},
};
use maud::Markup;
use tokio_stream::{
    StreamExt as _,
    wrappers::{BroadcastStream, WatchStream},
};

use crate::{
    forms, metrics,
    problem::ProblemDetails,
    security::{csrf_guard, security_headers},
    views::{self, DashboardModel, HistoryModel, JobModel, JobRow, Notice},
};

/// Largest request body any route accepts.
const MAX_BODY_BYTES: usize = 64 * 1024;
const SHELF_LEN: u32 = 30;
const RECENT_LEN: u32 = 8;
const ATTENTION_FAILED_LEN: u32 = 5;
const ACTIVE_LEN: u32 = 50;

/// Everything the handlers talk to.
pub struct WebDeps {
    pub jobs: Arc<dyn JobRepository>,
    pub catalog: Arc<dyn BookCatalog>,
    /// Series names, for series jobs.
    pub metadata: Arc<dyn MetadataStore>,
    pub admin: Arc<JobAdmin>,
    /// The delete buttons.
    pub library: Arc<fanfold_app::library::LibraryAdmin>,
    /// The library page: every book with its metadata.
    pub listing: Arc<dyn LibraryCatalog>,
    pub events: Arc<dyn EventSource>,
    pub site: Arc<dyn SiteStatusSource>,
    pub clock: Arc<dyn Clock>,
    pub stats: Arc<WorkerStats>,
    pub mail: Arc<MailStats>,
    pub readiness: Arc<dyn Readiness>,
    /// Flips to `true` at shutdown so open event streams end and the server can
    /// drain; otherwise a graceful shutdown would wait on them forever.
    pub shutdown: tokio::sync::watch::Receiver<bool>,
    /// Shown as "x of N" on a job page.
    pub max_attempts: u32,
    pub page_size: u32,
    pub version: &'static str,
}

impl std::fmt::Debug for WebDeps {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WebDeps").finish_non_exhaustive()
    }
}

type Deps = Arc<WebDeps>;

// ------------------------------------------------------------------- responses

fn wants_html(headers: &HeaderMap) -> bool {
    headers
        .get(header::ACCEPT)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|accept| accept.contains("text/html"))
}

fn html_response(status: StatusCode, markup: &Markup) -> Response {
    let mut response = (status, markup.0.clone()).into_response();
    let headers = response.headers_mut();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/html; charset=utf-8"),
    );
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    headers.insert(header::VARY, HeaderValue::from_static("x-fragment"));
    response
}

/// Errors are RFC 9457 problem documents; browsers asking for HTML get the same
/// problem rendered as a page, with the same status code.
fn problem_response(headers: &HeaderMap, problem: ProblemDetails) -> Response {
    if wants_html(headers) {
        let mut response = html_response(problem.status_code(), &views::error_page(&problem));
        // The same header the JSON form carries: clients back off identically.
        if let Some(secs) = problem.retry_after_secs {
            response
                .headers_mut()
                .insert(header::RETRY_AFTER, HeaderValue::from(secs));
        }
        response
    } else {
        problem.into_axum_response()
    }
}

fn is_fragment(headers: &HeaderMap) -> bool {
    headers.contains_key("x-fragment")
}

/// A full page, or just the live region when asked for a fragment.
fn page_or_fragment(headers: &HeaderMap, page: &Markup, fragment: &Markup) -> Response {
    html_response(
        StatusCode::OK,
        if is_fragment(headers) { fragment } else { page },
    )
}

fn parse_job_id(raw: &str) -> Result<JobId, ProblemDetails> {
    raw.parse::<i64>()
        .ok()
        .filter(|id| *id > 0)
        .map(JobId::new)
        .ok_or_else(|| {
            ProblemDetails::about_blank(StatusCode::NOT_FOUND)
                .with_detail("That is not a valid job id.")
        })
}

fn internal(error: &impl std::fmt::Display) -> ProblemDetails {
    tracing::error!(%error, "request failed");
    ProblemDetails::from(&fanfold_app::error::AppError::Repo(
        fanfold_app::error::RepoError::Backend(error.to_string()),
    ))
}

// -------------------------------------------------------------------- data

async fn rows(deps: &WebDeps, jobs: Vec<Job>) -> Result<Vec<JobRow>, ProblemDetails> {
    let mut works: Vec<WorkId> = Vec::new();
    let mut series: Vec<SeriesId> = Vec::new();
    for job in &jobs {
        match job.target {
            Target::Work(w) => works.push(w),
            Target::Series(s) => series.push(s),
        }
    }
    let titles: HashMap<WorkId, String> = deps
        .catalog
        .titles(&works)
        .await
        .map_err(|e| internal(&e))?;
    let names: HashMap<SeriesId, String> = deps
        .metadata
        .series_names(&series)
        .await
        .map_err(|e| internal(&e))?;
    Ok(jobs
        .into_iter()
        .map(|job| JobRow {
            title: match job.target {
                Target::Work(w) => titles.get(&w).cloned(),
                Target::Series(s) => names.get(&s).cloned(),
            },
            job,
        })
        .collect())
}

fn query_pairs(raw: Option<String>) -> Vec<(String, String)> {
    forms::pairs(raw.unwrap_or_default().as_bytes())
}

// ------------------------------------------------------------------- handlers

async fn dashboard(
    State(deps): State<Deps>,
    headers: HeaderMap,
    RawQuery(query): RawQuery,
) -> Response {
    match dashboard_inner(&deps, &headers, query).await {
        Ok(response) => response,
        Err(problem) => problem_response(&headers, problem),
    }
}

async fn dashboard_inner(
    deps: &WebDeps,
    headers: &HeaderMap,
    query: Option<String>,
) -> Result<Response, ProblemDetails> {
    let now = deps.clock.now();
    let fetch = |filter, limit| async move {
        deps.jobs
            .list(filter, limit, 0)
            .await
            .map_err(|e| internal(&e))
    };
    let active = fetch(JobFilter::Active, ACTIVE_LEN).await?;
    let failed = fetch(JobFilter::Failed, ATTENTION_FAILED_LEN).await?;
    let mut finished = fetch(JobFilter::Finished, SHELF_LEN).await?;
    let counts = deps.jobs.counts().await.map_err(|e| internal(&e))?;

    let (retrying, in_progress): (Vec<Job>, Vec<Job>) =
        active.into_iter().partition(|j| j.last_error.is_some());
    let mut attention: Vec<Job> = retrying.into_iter().chain(failed).collect();
    attention.sort_by_key(|j| std::cmp::Reverse((j.updated_at, j.id)));
    // Failures already listed under "Needs attention" are not repeated below.
    let shown: std::collections::HashSet<JobId> = attention.iter().map(|j| j.id).collect();
    let recent: Vec<Job> = finished
        .iter()
        .filter(|j| !shown.contains(&j.id))
        .take(RECENT_LEN as usize)
        .cloned()
        .collect();
    finished.reverse(); // the shelf reads oldest to newest

    let model = DashboardModel {
        now,
        site: deps.site.site_status(),
        counts,
        attention: rows(deps, attention).await?,
        in_progress: rows(deps, in_progress).await?,
        recent: rows(deps, recent).await?,
        shelf: rows(deps, finished).await?,
    };
    let notice = Notice::from_query(&query_pairs(query));
    Ok(page_or_fragment(
        headers,
        &views::dashboard_page(&model, notice),
        &views::dashboard(&model),
    ))
}

async fn history(
    State(deps): State<Deps>,
    headers: HeaderMap,
    RawQuery(query): RawQuery,
) -> Response {
    match history_inner(&deps, &headers, query).await {
        Ok(response) => response,
        Err(problem) => problem_response(&headers, problem),
    }
}

async fn history_inner(
    deps: &WebDeps,
    headers: &HeaderMap,
    query: Option<String>,
) -> Result<Response, ProblemDetails> {
    let pairs = query_pairs(query);
    let filter = forms::first(&pairs, "filter")
        .and_then(JobFilter::parse)
        .unwrap_or(JobFilter::All);
    let page = forms::first(&pairs, "page")
        .and_then(|p| p.parse::<u32>().ok())
        .filter(|p| *p >= 1)
        .unwrap_or(1);
    let size = deps.page_size.max(1);
    let offset = (page - 1).saturating_mul(size);

    // One extra row tells us whether there is a next page.
    let mut jobs = deps
        .jobs
        .list(filter, size + 1, offset)
        .await
        .map_err(|e| internal(&e))?;
    let has_next = jobs.len() > size as usize;
    jobs.truncate(size as usize);
    let model = HistoryModel {
        now: deps.clock.now(),
        site: deps.site.site_status(),
        counts: deps.jobs.counts().await.map_err(|e| internal(&e))?,
        filter,
        page,
        rows: rows(deps, jobs).await?,
        has_next,
    };
    let notice = Notice::from_query(&pairs);
    Ok(page_or_fragment(
        headers,
        &views::history_page(&model, notice),
        &views::history(&model),
    ))
}

async fn library_page(
    State(deps): State<Deps>,
    headers: HeaderMap,
    RawQuery(query): RawQuery,
) -> Response {
    match library_inner(&deps, &headers, query).await {
        Ok(response) => response,
        Err(problem) => problem_response(&headers, problem),
    }
}

/// Longest search accepted; anything longer is cut (nobody types a 200-character title).
const MAX_SEARCH_CHARS: usize = 200;

async fn library_inner(
    deps: &WebDeps,
    headers: &HeaderMap,
    query: Option<String>,
) -> Result<Response, ProblemDetails> {
    let pairs = query_pairs(query);
    let page = forms::first(&pairs, "page")
        .and_then(|p| p.parse::<u32>().ok())
        .filter(|p| *p >= 1)
        .unwrap_or(1);
    let size = deps.page_size.max(1);
    let library_query = LibraryQuery {
        search: forms::first(&pairs, "q")
            .map(|q| q.trim().chars().take(MAX_SEARCH_CHARS).collect::<String>())
            .filter(|q| !q.is_empty()),
        status: forms::first(&pairs, "status")
            .and_then(LibraryStatus::parse)
            .unwrap_or_default(),
        sort: forms::first(&pairs, "sort")
            .and_then(LibrarySort::parse)
            .unwrap_or_default(),
        limit: size,
        offset: (page - 1).saturating_mul(size),
    };
    let result = deps
        .listing
        .library(&library_query)
        .await
        .map_err(|e| internal(&e))?;
    let model = views::library::LibraryModel {
        now: deps.clock.now(),
        site: deps.site.site_status(),
        query: library_query,
        page,
        result,
    };
    let notice = Notice::from_query(&pairs);
    Ok(page_or_fragment(
        headers,
        &views::library::library_page(&model, notice),
        &views::library::library(&model),
    ))
}

async fn book_page(
    State(deps): State<Deps>,
    headers: HeaderMap,
    Path(raw): Path<String>,
    RawQuery(query): RawQuery,
) -> Response {
    match book_inner(&deps, &headers, &raw, query).await {
        Ok(response) => response,
        Err(problem) => problem_response(&headers, problem),
    }
}

async fn book_inner(
    deps: &WebDeps,
    headers: &HeaderMap,
    raw: &str,
    query: Option<String>,
) -> Result<Response, ProblemDetails> {
    let work = parse_ao3_id(raw)
        .and_then(WorkId::new)
        .ok_or_else(|| invalid_id("work"))?;
    let entry = deps
        .listing
        .entry(work)
        .await
        .map_err(|e| internal(&e))?
        .ok_or_else(|| {
            ProblemDetails::about_blank(StatusCode::NOT_FOUND)
                .with_detail("That work is not in your library.")
        })?;
    let state = deps
        .metadata
        .work_state(work)
        .await
        .map_err(|e| internal(&e))?;
    let model = views::library::BookModel {
        now: deps.clock.now(),
        site: deps.site.site_status(),
        entry,
        state,
    };
    let notice = Notice::from_query(&query_pairs(query));
    Ok(page_or_fragment(
        headers,
        &views::library::book_page(&model, notice),
        &views::library::book(&model),
    ))
}

async fn job_page(
    State(deps): State<Deps>,
    headers: HeaderMap,
    Path(id): Path<String>,
    RawQuery(query): RawQuery,
) -> Response {
    match job_inner(&deps, &headers, &id, query).await {
        Ok(response) => response,
        Err(problem) => problem_response(&headers, problem),
    }
}

async fn job_inner(
    deps: &WebDeps,
    headers: &HeaderMap,
    raw_id: &str,
    query: Option<String>,
) -> Result<Response, ProblemDetails> {
    let id = parse_job_id(raw_id)?;
    let job = deps
        .jobs
        .get(id)
        .await
        .map_err(|e| internal(&e))?
        .ok_or_else(|| ProblemDetails::from(&AdminError::NotFound(id)))?;
    let book = match job.target {
        Target::Work(work) => deps.catalog.get(work).await.map_err(|e| internal(&e))?,
        Target::Series(_) => None,
    };
    let logs = deps.jobs.logs(id).await.map_err(|e| internal(&e))?;
    let title = match job.target {
        Target::Work(_) => book.as_ref().map(|b| b.title.clone()),
        Target::Series(series) => deps
            .metadata
            .series_names(&[series])
            .await
            .map_err(|e| internal(&e))?
            .remove(&series),
    };
    let model = JobModel {
        now: deps.clock.now(),
        site: deps.site.site_status(),
        row: JobRow { job, title },
        book,
        logs,
        max_attempts: deps.max_attempts,
    };
    let notice = Notice::from_query(&query_pairs(query));
    Ok(page_or_fragment(
        headers,
        &views::job_page(&model, notice),
        &views::job_detail(&model),
    ))
}

// ------------------------------------------------------------------- actions

fn admin_problem(error: &AdminError) -> ProblemDetails {
    if let AdminError::Repo(e) = error {
        tracing::error!(error = %e, "admin action failed");
    }
    ProblemDetails::from(error)
}

async fn add_job(State(deps): State<Deps>, headers: HeaderMap, body: Bytes) -> Response {
    let pairs = forms::pairs(&body);
    let url = forms::first(&pairs, "url").unwrap_or_default();
    match deps.admin.add_url(url).await {
        Ok(fanfold_app::ports::EnqueueOutcome::Created(job)) => {
            Redirect::to(&format!("/?added={}", job.id)).into_response()
        }
        Ok(fanfold_app::ports::EnqueueOutcome::AlreadyActive(id)) => {
            Redirect::to(&format!("/?queued={id}")).into_response()
        }
        Err(e) => problem_response(&headers, admin_problem(&e)),
    }
}

/// A positive AO3 id from the path.
fn parse_ao3_id(raw: &str) -> Option<u64> {
    raw.parse::<u64>().ok().filter(|id| *id > 0)
}

fn invalid_id(what: &str) -> ProblemDetails {
    ProblemDetails::about_blank(StatusCode::NOT_FOUND)
        .with_detail(format!("That is not a valid {what} id."))
}

/// Where to go back to after a delete: the job page it was pressed on, the library when it
/// was pressed there, else the dashboard.
fn back_to(pairs: &[(String, String)], query: &str) -> (Option<JobId>, String) {
    let job = forms::job_ids(pairs, "job").first().copied();
    let to = match job {
        Some(job) => format!("/jobs/{job}?{query}"),
        None if forms::first(pairs, "back") == Some("library") => format!("/library?{query}"),
        None => format!("/?{query}"),
    };
    (job, to)
}

async fn delete_work(
    State(deps): State<Deps>,
    headers: HeaderMap,
    Path(raw): Path<String>,
    body: Bytes,
) -> Response {
    let Some(work) = parse_ao3_id(&raw).and_then(WorkId::new) else {
        return problem_response(&headers, invalid_id("work"));
    };
    let pairs = forms::pairs(&body);
    let (job, _) = back_to(&pairs, "");
    match deps.library.delete_book(work, job).await {
        Ok(deleted) => {
            let query = format!("deleted_book={}", u8::from(deleted.file.is_some()));
            Redirect::to(&back_to(&pairs, &query).1).into_response()
        }
        Err(e) => problem_response(&headers, ProblemDetails::from(&e)),
    }
}

async fn delete_series(
    State(deps): State<Deps>,
    headers: HeaderMap,
    Path(raw): Path<String>,
    body: Bytes,
) -> Response {
    let Some(series) = parse_ao3_id(&raw).and_then(SeriesId::new) else {
        return problem_response(&headers, invalid_id("series"));
    };
    let pairs = forms::pairs(&body);
    let (job, _) = back_to(&pairs, "");
    match deps.library.delete_series(series, job).await {
        Ok(done) => {
            let deleted = done.deleted.iter().filter(|d| d.file.is_some()).count();
            let query = format!("deleted_series={deleted}&kept={}", done.kept.len());
            Redirect::to(&back_to(&pairs, &query).1).into_response()
        }
        Err(e) => problem_response(&headers, ProblemDetails::from(&e)),
    }
}

async fn retry_job(
    State(deps): State<Deps>,
    headers: HeaderMap,
    Path(raw): Path<String>,
) -> Response {
    let result = match parse_job_id(&raw) {
        Ok(id) => deps
            .admin
            .retry(id)
            .await
            .map(|_| id)
            .map_err(|e| admin_problem(&e)),
        Err(problem) => Err(problem),
    };
    match result {
        Ok(id) => Redirect::to(&format!("/jobs/{id}?retried_one={id}")).into_response(),
        Err(problem) => problem_response(&headers, problem),
    }
}

async fn force_retry_job(
    State(deps): State<Deps>,
    headers: HeaderMap,
    Path(raw): Path<String>,
) -> Response {
    let result = match parse_job_id(&raw) {
        Ok(id) => deps
            .admin
            .force_retry(id)
            .await
            .map(|_| id)
            .map_err(|e| admin_problem(&e)),
        Err(problem) => Err(problem),
    };
    match result {
        Ok(id) => Redirect::to(&format!("/jobs/{id}?forced={id}")).into_response(),
        Err(problem) => problem_response(&headers, problem),
    }
}

async fn cancel_job(
    State(deps): State<Deps>,
    headers: HeaderMap,
    Path(raw): Path<String>,
) -> Response {
    let result = match parse_job_id(&raw) {
        Ok(id) => deps
            .admin
            .cancel(id)
            .await
            .map(|_| id)
            .map_err(|e| admin_problem(&e)),
        Err(problem) => Err(problem),
    };
    match result {
        Ok(id) => Redirect::to(&format!("/jobs/{id}?cancelled={id}")).into_response(),
        Err(problem) => problem_response(&headers, problem),
    }
}

fn bulk_redirect(report: &fanfold_app::admin::BulkReport) -> Response {
    Redirect::to(&format!(
        "/jobs?filter=failed&retried={}&skipped={}",
        report.retried.len(),
        report.skipped.len()
    ))
    .into_response()
}

async fn retry_selected(State(deps): State<Deps>, headers: HeaderMap, body: Bytes) -> Response {
    let ids = forms::job_ids(&forms::pairs(&body), "id");
    if ids.is_empty() {
        let problem = ProblemDetails::about_blank(StatusCode::UNPROCESSABLE_ENTITY)
            .with_detail("Select at least one job to retry.");
        return problem_response(&headers, problem);
    }
    bulk_redirect(&deps.admin.retry_many(&ids).await)
}

async fn retry_failed(State(deps): State<Deps>, headers: HeaderMap) -> Response {
    match deps.admin.retry_all_failed().await {
        Ok(report) => bulk_redirect(&report),
        Err(e) => problem_response(&headers, admin_problem(&e)),
    }
}

// -------------------------------------------------------------------- events

fn sse_data(
    event: &Result<Event, tokio_stream::wrappers::errors::BroadcastStreamRecvError>,
) -> String {
    match event {
        Ok(Event::JobChanged(id)) => format!("job {id}"),
        Ok(Event::LogAppended(id)) => format!("log {id}"),
        // A slow reader skipped events, or the source reconnected: refetch everything.
        Ok(Event::Resync) | Err(_) => "resync".to_owned(),
    }
}

/// What the event stream is made of: change notifications, plus the shutdown flag.
enum Tick {
    Change(Result<Event, tokio_stream::wrappers::errors::BroadcastStreamRecvError>),
    Shutdown(bool),
}

async fn events(State(deps): State<Deps>) -> Response {
    let changes = BroadcastStream::new(deps.events.subscribe()).map(Tick::Change);
    let shutdown = WatchStream::new(deps.shutdown.clone()).map(Tick::Shutdown);
    let stream = changes
        .merge(shutdown)
        // End the stream at shutdown so graceful shutdown is not held open by it.
        .take_while(|tick| !matches!(tick, Tick::Shutdown(true)))
        .filter_map(|tick| match tick {
            Tick::Change(event) => Some(Ok::<_, Infallible>(
                SseEvent::default().event("change").data(sse_data(&event)),
            )),
            Tick::Shutdown(_) => None,
        });
    let mut response = Sse::new(stream)
        .keep_alive(
            KeepAlive::new()
                .interval(Duration::from_secs(15))
                .text("keep-alive"),
        )
        .into_response();
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    // Stop proxies from buffering the stream.
    response
        .headers_mut()
        .insert("x-accel-buffering", HeaderValue::from_static("no"));
    response
}

// -------------------------------------------------------------------- assets

fn asset(content_type: &'static str, body: &'static str) -> Response {
    let mut response = (StatusCode::OK, body).into_response();
    let headers = response.headers_mut();
    headers.insert(header::CONTENT_TYPE, HeaderValue::from_static(content_type));
    headers.insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("public, max-age=300"),
    );
    response
}

async fn css() -> Response {
    asset("text/css; charset=utf-8", include_str!("../assets/app.css"))
}

async fn js() -> Response {
    asset(
        "text/javascript; charset=utf-8",
        include_str!("../assets/app.js"),
    )
}

async fn icon() -> Response {
    asset("image/svg+xml", include_str!("../assets/icon.svg"))
}

async fn not_found(headers: HeaderMap) -> Response {
    problem_response(&headers, ProblemDetails::about_blank(StatusCode::NOT_FOUND))
}

// -------------------------------------------------------------------- routers

/// The user interface. Put it behind authentication; it never authenticates itself.
pub fn ui_router(deps: Deps) -> Router {
    Router::new()
        .route("/", get(dashboard))
        .route("/jobs", get(history).post(add_job))
        .route("/jobs/retry", post(retry_selected))
        .route("/jobs/retry-failed", post(retry_failed))
        .route("/library", get(library_page))
        .route("/library/{id}", get(book_page))
        .route("/jobs/{id}", get(job_page))
        .route("/jobs/{id}/retry", post(retry_job))
        .route("/jobs/{id}/force-retry", post(force_retry_job))
        .route("/jobs/{id}/cancel", post(cancel_job))
        .route("/works/{id}/delete", post(delete_work))
        .route("/series/{id}/delete", post(delete_series))
        .route("/events", get(events))
        .route("/static/app.css", get(css))
        .route("/static/app.js", get(js))
        .route("/static/icon.svg", get(icon))
        .fallback(not_found)
        .layer(DefaultBodyLimit::max(MAX_BODY_BYTES))
        .layer(middleware::from_fn(csrf_guard))
        .layer(middleware::from_fn(security_headers))
        .with_state(deps)
}

async fn healthz() -> &'static str {
    "ok"
}

async fn readyz(State(deps): State<Deps>) -> Response {
    match deps.readiness.check().await {
        Ok(()) => (StatusCode::OK, "ready").into_response(),
        Err(e) => {
            tracing::warn!(error = %e, "not ready");
            ProblemDetails::about_blank(StatusCode::SERVICE_UNAVAILABLE)
                .with_detail("The database is not reachable.")
                .into_axum_response()
        }
    }
}

async fn metrics_endpoint(State(deps): State<Deps>) -> Response {
    let counts = match deps.jobs.counts().await {
        Ok(counts) => counts,
        Err(e) => return problem_response(&HeaderMap::new(), internal(&e)),
    };
    let body = metrics::render(
        &deps.stats.snapshot(),
        &deps.mail.snapshot(),
        &counts,
        deps.site.site_status(),
        deps.version,
    );
    let mut response = (StatusCode::OK, body).into_response();
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static(metrics::CONTENT_TYPE),
    );
    response
}

/// Liveness, readiness and metrics, for Kubernetes and Prometheus. No auth: serve it
/// on a separate, cluster-internal port.
pub fn ops_router(deps: Deps) -> Router {
    Router::new()
        .route("/healthz", get(healthz))
        .route("/readyz", get(readyz))
        .route("/metrics", get(metrics_endpoint))
        .fallback(not_found)
        .layer(middleware::from_fn(security_headers))
        .with_state(deps)
}
