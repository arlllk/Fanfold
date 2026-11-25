//! Routes and handlers.
//!
//! Two routers: the **UI** (pages, actions, live events), which sits behind the
//! gateway's login, and **ops** (`/healthz`, `/readyz`, `/metrics`), which Kubernetes
//! and Prometheus reach directly on a separate port.
//!
//! Pages are rendered completely on the server. htmx re-requests the same URL (a live
//! update, a search as you type) with `HX-Request: true` and gets only the page's live
//! region back, so there is one rendering path.

// Problem documents are only built on error paths, so the size of an `Err` is
// irrelevant to throughput; boxing them would only add noise to every handler.
#![allow(clippy::result_large_err)]

use std::{collections::HashMap, convert::Infallible, sync::Arc, time::Duration};

use axum::{
    Router,
    body::Bytes,
    extract::{DefaultBodyLimit, Path, RawQuery, State},
    http::{HeaderMap, HeaderValue, StatusCode, Uri, header},
    middleware,
    response::{
        IntoResponse, Redirect, Response,
        sse::{Event as SseEvent, KeepAlive, Sse},
    },
    routing::{get, post},
};
use fanfold_app::{
    admin::{AdminError, JobAdmin},
    browse::{BrowseError, GroupKind, LibraryBrowser, LibrarySort},
    ports::{
        BookCatalog, Clock, Event, EventSource, JobFilter, JobRepository, LibraryCatalog,
        MetadataStore, Readiness, SiteStatusSource,
    },
    search::{Clause, Flag, SearchQuery, Token},
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
    assets::{self, Lookup},
    forms, metrics,
    problem::ProblemDetails,
    security::{csrf_guard, security_headers},
    views::{
        self, DashboardModel, HistoryModel, JobModel, JobRow, Notice,
        library::{self, BooksModel, GroupsModel, LibraryFrame, SeriesListModel, Tab},
    },
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
    /// One book's page.
    pub listing: Arc<dyn LibraryCatalog>,
    /// The library pages: search, series, authors, fandoms.
    pub browser: Arc<LibraryBrowser>,
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
    /// When book pages offer "Open in Moon+ Reader".
    pub moon_reader: crate::views::library::MoonReaderButton,
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
    headers.insert(header::VARY, HeaderValue::from_static("hx-request"));
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

/// Whether htmx asked (a live update, a search as you type): it gets the live region only.
fn is_fragment(headers: &HeaderMap) -> bool {
    headers
        .get("hx-request")
        .is_some_and(|v| v.as_bytes() == b"true")
        && !headers.contains_key("hx-history-restore-request")
}

/// A full page, or just its live region when htmx asks.
fn respond(headers: &HeaderMap, screen: &views::Screen<'_>, notice: Option<Notice>) -> Response {
    if is_fragment(headers) {
        html_response(StatusCode::OK, &views::fragment(screen))
    } else {
        html_response(StatusCode::OK, &views::full_page(screen, notice))
    }
}

/// The path and query a page was asked for: what its live region refreshes from.
fn page_url(uri: &Uri) -> String {
    uri.path_and_query()
        .map_or_else(|| uri.path().to_owned(), ToString::to_string)
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
    uri: Uri,
    RawQuery(query): RawQuery,
) -> Response {
    match dashboard_inner(&deps, &headers, &page_url(&uri), query).await {
        Ok(response) => response,
        Err(problem) => problem_response(&headers, problem),
    }
}

async fn dashboard_inner(
    deps: &WebDeps,
    headers: &HeaderMap,
    url: &str,
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
    Ok(respond(
        headers,
        &views::dashboard_screen(&model, url),
        notice,
    ))
}

async fn history(
    State(deps): State<Deps>,
    headers: HeaderMap,
    uri: Uri,
    RawQuery(query): RawQuery,
) -> Response {
    match history_inner(&deps, &headers, &page_url(&uri), query).await {
        Ok(response) => response,
        Err(problem) => problem_response(&headers, problem),
    }
}

async fn history_inner(
    deps: &WebDeps,
    headers: &HeaderMap,
    url: &str,
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
    Ok(respond(
        headers,
        &views::history_screen(&model, url),
        notice,
    ))
}

async fn library_page(
    State(deps): State<Deps>,
    headers: HeaderMap,
    uri: Uri,
    RawQuery(query): RawQuery,
) -> Response {
    let url = page_url(&uri);
    match library_inner(&deps, &headers, Tab::Books, &url, query).await {
        Ok(response) => response,
        Err(problem) => problem_response(&headers, problem),
    }
}

async fn library_series_page(
    State(deps): State<Deps>,
    headers: HeaderMap,
    uri: Uri,
    RawQuery(query): RawQuery,
) -> Response {
    let url = page_url(&uri);
    match library_inner(&deps, &headers, Tab::Series, &url, query).await {
        Ok(response) => response,
        Err(problem) => problem_response(&headers, problem),
    }
}

async fn library_authors_page(
    State(deps): State<Deps>,
    headers: HeaderMap,
    uri: Uri,
    RawQuery(query): RawQuery,
) -> Response {
    let url = page_url(&uri);
    match library_inner(&deps, &headers, Tab::Authors, &url, query).await {
        Ok(response) => response,
        Err(problem) => problem_response(&headers, problem),
    }
}

async fn library_fandoms_page(
    State(deps): State<Deps>,
    headers: HeaderMap,
    uri: Uri,
    RawQuery(query): RawQuery,
) -> Response {
    let url = page_url(&uri);
    match library_inner(&deps, &headers, Tab::Fandoms, &url, query).await {
        Ok(response) => response,
        Err(problem) => problem_response(&headers, problem),
    }
}

/// Longest search accepted; anything longer is cut (nobody types a 500-character search).
const MAX_SEARCH_CHARS: usize = 500;

fn browse_problem(error: &BrowseError) -> ProblemDetails {
    internal(error)
}

/// The search in a library URL: `q`, plus the completion filter of older links (`status`).
fn library_query(pairs: &[(String, String)]) -> SearchQuery {
    let text: String = forms::first(pairs, "q")
        .unwrap_or("")
        .chars()
        .take(MAX_SEARCH_CHARS)
        .collect();
    let query = SearchQuery::parse(&text);
    let legacy = match forms::first(pairs, "status") {
        Some("complete") => Some(Flag::Complete),
        Some("in_progress") => Some(Flag::InProgress),
        _ => None,
    };
    match legacy {
        Some(flag) => query.with(Token::new(Clause::Is(flag)), |_| false),
        None => query,
    }
}

async fn library_inner(
    deps: &WebDeps,
    headers: &HeaderMap,
    tab: Tab,
    url: &str,
    query: Option<String>,
) -> Result<Response, ProblemDetails> {
    let pairs = query_pairs(query);
    let search = library_query(&pairs);
    let browser = &deps.browser;
    let fail = |e: BrowseError| browse_problem(&e);
    let frame = LibraryFrame {
        now: deps.clock.now(),
        site: deps.site.site_status(),
        tab,
        totals: browser.totals().await.map_err(fail)?,
        suggestions: browser.suggest(&search).await.map_err(fail)?,
        query: search,
    };
    let notice = Notice::from_query(&pairs);
    let response = match tab {
        Tab::Books => {
            let page = forms::first(&pairs, "page")
                .and_then(|p| p.parse::<u32>().ok())
                .filter(|p| *p >= 1)
                .unwrap_or(1);
            let size = deps.page_size.max(1);
            let offset = (page - 1).saturating_mul(size);
            let sort = forms::first(&pairs, "sort").and_then(LibrarySort::parse);
            let result = browser
                .books(&frame.query, sort, offset, size)
                .await
                .map_err(fail)?;
            let model = BooksModel {
                frame,
                sort,
                page,
                offset,
                result,
            };
            respond(headers, &library::books_screen(&model, url), notice)
        }
        Tab::Series => {
            let series = browser.series(&frame.query).await.map_err(fail)?;
            let model = SeriesListModel { frame, series };
            respond(headers, &library::series_list_screen(&model, url), notice)
        }
        Tab::Authors | Tab::Fandoms => {
            let kind = if tab == Tab::Authors {
                GroupKind::Author
            } else {
                GroupKind::Fandom
            };
            let groups = browser.groups(kind, &frame.query).await.map_err(fail)?;
            let model = GroupsModel {
                frame,
                kind,
                groups,
            };
            respond(headers, &library::groups_screen(&model, url), notice)
        }
    };
    Ok(response)
}

async fn series_page(
    State(deps): State<Deps>,
    headers: HeaderMap,
    uri: Uri,
    Path(raw): Path<String>,
) -> Response {
    let url = page_url(&uri);
    let result = async {
        let id = parse_ao3_id(&raw)
            .and_then(SeriesId::new)
            .ok_or_else(|| invalid_id("series"))?;
        let series = deps
            .browser
            .one_series(id)
            .await
            .map_err(|e| browse_problem(&e))?
            .ok_or_else(|| {
                ProblemDetails::about_blank(StatusCode::NOT_FOUND).with_detail(
                    "No book in your library belongs to that series, and it is not followed.",
                )
            })?;
        let model = library::SeriesModel {
            now: deps.clock.now(),
            site: deps.site.site_status(),
            series,
        };
        Ok::<_, ProblemDetails>(respond(
            &headers,
            &library::series_screen(&model, &url),
            None,
        ))
    }
    .await;
    result.unwrap_or_else(|problem| problem_response(&headers, problem))
}

async fn book_page(
    State(deps): State<Deps>,
    headers: HeaderMap,
    uri: Uri,
    Path(raw): Path<String>,
    RawQuery(query): RawQuery,
) -> Response {
    let url = page_url(&uri);
    match book_inner(&deps, &headers, &url, &raw, query).await {
        Ok(response) => response,
        Err(problem) => problem_response(&headers, problem),
    }
}

async fn book_inner(
    deps: &WebDeps,
    headers: &HeaderMap,
    url: &str,
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
    let model = library::BookModel {
        now: deps.clock.now(),
        site: deps.site.site_status(),
        entry,
        state,
        moon_reader: deps.moon_reader,
    };
    let notice = Notice::from_query(&query_pairs(query));
    Ok(respond(headers, &library::book_screen(&model, url), notice))
}

async fn job_page(
    State(deps): State<Deps>,
    headers: HeaderMap,
    uri: Uri,
    Path(id): Path<String>,
    RawQuery(query): RawQuery,
) -> Response {
    match job_inner(&deps, &headers, &page_url(&uri), &id, query).await {
        Ok(response) => response,
        Err(problem) => problem_response(&headers, problem),
    }
}

async fn job_inner(
    deps: &WebDeps,
    headers: &HeaderMap,
    url: &str,
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
    let name = model.row.name();
    Ok(respond(
        headers,
        &views::job_screen(&model, &name, url),
        notice,
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

/// The SSE messages one change becomes. Named, so pages listen without filtering in script:
/// every page refreshes on `refresh`, a job's page only on `job-<id>`, all of them on
/// `resync` (a slow reader skipped events, or the source reconnected).
fn sse_events(
    event: &Result<Event, tokio_stream::wrappers::errors::BroadcastStreamRecvError>,
) -> Vec<SseEvent> {
    match event {
        Ok(Event::JobChanged(id)) => vec![
            SseEvent::default()
                .event("refresh")
                .data(format!("job {id}")),
            SseEvent::default().event(format!("job-{id}")).data("job"),
        ],
        Ok(Event::LogAppended(id)) => vec![
            SseEvent::default()
                .event("refresh")
                .data(format!("log {id}")),
            SseEvent::default().event(format!("job-{id}")).data("log"),
        ],
        Ok(Event::Resync) | Err(_) => vec![SseEvent::default().event("resync").data("resync")],
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
    let ticks = changes
        .merge(shutdown)
        // End the stream at shutdown so graceful shutdown is not held open by it.
        .take_while(|tick| !matches!(tick, Tick::Shutdown(true)));
    let stream = futures_util::StreamExt::flat_map(ticks, |tick| {
        let events = match tick {
            Tick::Change(event) => sse_events(&event),
            Tick::Shutdown(_) => Vec::new(),
        };
        futures_util::stream::iter(events.into_iter().map(Ok::<_, Infallible>))
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

/// A file under `/static/`. Its current hashed name is cached for a year (`immutable`, by
/// browsers and the CDN alike); a plain or stale name gets the current file to revalidate
/// every time (`no-cache`), which a CDN does not store. See [`crate::assets`].
async fn asset(headers: HeaderMap, Path(path): Path<String>) -> Response {
    let (file, cache) = match assets::lookup(&path) {
        Some(Lookup::Current(file)) => (file, "public, max-age=31536000, immutable"),
        Some(Lookup::Moved(file)) => (file, "no-cache"),
        None => return not_found(headers).await,
    };
    let fresh = headers
        .get(header::IF_NONE_MATCH)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|tags| {
            tags.split(',')
                .any(|t| t.trim() == file.etag || t.trim() == "*")
        });
    let mut response = if fresh {
        StatusCode::NOT_MODIFIED.into_response()
    } else {
        (StatusCode::OK, file.body.as_slice()).into_response()
    };
    let out = response.headers_mut();
    out.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static(file.content_type),
    );
    out.insert(header::CACHE_CONTROL, HeaderValue::from_static(cache));
    if let Ok(etag) = HeaderValue::from_str(&file.etag) {
        out.insert(header::ETAG, etag);
    }
    response
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
        .route("/library/series", get(library_series_page))
        .route("/library/series/{id}", get(series_page))
        .route("/library/authors", get(library_authors_page))
        .route("/library/fandoms", get(library_fandoms_page))
        .route("/library/{id}", get(book_page))
        .route("/jobs/{id}", get(job_page))
        .route("/jobs/{id}/retry", post(retry_job))
        .route("/jobs/{id}/force-retry", post(force_retry_job))
        .route("/jobs/{id}/cancel", post(cancel_job))
        .route("/works/{id}/delete", post(delete_work))
        .route("/series/{id}/delete", post(delete_series))
        .route("/events", get(events))
        .route("/static/{*path}", get(asset))
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
