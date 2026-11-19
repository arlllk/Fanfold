//! HTML, rendered on the server with `maud`.
//!
//! Everything interpolated with `(expr)` is HTML-escaped. Titles, log lines and error
//! messages all originate from a third-party site, so that is not optional. There are
//! no inline styles, scripts or event handlers: the CSP forbids them.

use fanfold_app::{
    ports::{BookRecord, JobCounts, JobFilter, LogEntry, LogLevel, SiteStatus},
    session::SessionView,
    worker::ATTEMPT_START_PREFIX,
};
use fanfold_domain::{
    Timestamp,
    job::{Job, JobId, JobSource, JobState, Target},
    session::CookieStatus,
};
use maud::{DOCTYPE, Markup, html};

use crate::{
    format::{clock, first_line, iso, relative, site_summary, status_of, truncate},
    problem::ProblemDetails,
};

pub mod library;

// ----------------------------------------------------------------------- models

#[derive(Debug, Clone)]
pub struct JobRow {
    pub job: Job,
    pub title: Option<String>,
}

impl JobRow {
    fn name(&self) -> String {
        self.title.clone().unwrap_or_else(|| match self.job.target {
            Target::Work(w) => format!("Work #{w}"),
            Target::Series(s) => format!("Series #{s}"),
        })
    }

    /// The AO3 id as shown next to a known title: "#123" for a work, "series #45".
    fn id_label(&self) -> String {
        match self.job.target {
            Target::Work(w) => format!("#{w}"),
            Target::Series(s) => format!("series #{s}"),
        }
    }

    const fn is_series(&self) -> bool {
        matches!(self.job.target, Target::Series(_))
    }
}

#[derive(Debug)]
pub struct DashboardModel {
    pub now: Timestamp,
    pub site: SiteStatus,
    pub counts: JobCounts,
    pub attention: Vec<JobRow>,
    pub in_progress: Vec<JobRow>,
    pub recent: Vec<JobRow>,
    /// Oldest first.
    pub shelf: Vec<JobRow>,
}

#[derive(Debug)]
pub struct HistoryModel {
    pub now: Timestamp,
    pub site: SiteStatus,
    pub counts: JobCounts,
    pub filter: JobFilter,
    pub page: u32,
    pub rows: Vec<JobRow>,
    pub has_next: bool,
}

#[derive(Debug)]
pub struct JobModel {
    pub now: Timestamp,
    pub site: SiteStatus,
    pub row: JobRow,
    pub book: Option<BookRecord>,
    pub logs: Vec<LogEntry>,
    pub max_attempts: u32,
}

/// A one-off message after an action. Built only from numbers in the query string,
/// never from arbitrary text, so nothing user-supplied is reflected.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Notice {
    Added(JobId),
    AlreadyQueued(JobId),
    Retried(JobId),
    /// Queued again to download from scratch.
    ForceRetried(JobId),
    Cancelled(JobId),
    Bulk {
        retried: u32,
        skipped: u32,
    },
    /// Cookies were imported into the browser session.
    SessionImported {
        cookies: u32,
    },
    SessionCleared,
    /// A book was deleted (`false`: there was none to delete).
    DeletedBook(bool),
    /// A series was deleted: books deleted, books kept for another series.
    DeletedSeries {
        deleted: u32,
        kept: u32,
    },
}

impl Notice {
    #[must_use]
    pub fn from_query(pairs: &[(String, String)]) -> Option<Self> {
        let num = |key: &str| {
            pairs
                .iter()
                .find(|(k, _)| k == key)
                .and_then(|(_, v)| v.parse::<u32>().ok())
        };
        let id = |key: &str| {
            num(key)
                .filter(|n| *n > 0)
                .map(|n| JobId::new(i64::from(n)))
        };
        if let Some(job) = id("added") {
            Some(Self::Added(job))
        } else if let Some(job) = id("queued") {
            Some(Self::AlreadyQueued(job))
        } else if let Some(job) = id("retried_one") {
            Some(Self::Retried(job))
        } else if let Some(job) = id("forced") {
            Some(Self::ForceRetried(job))
        } else if let Some(job) = id("cancelled") {
            Some(Self::Cancelled(job))
        } else if let Some(deleted) = num("deleted_series") {
            Some(Self::DeletedSeries {
                deleted,
                kept: num("kept").unwrap_or(0),
            })
        } else if let Some(file) = num("deleted_book") {
            Some(Self::DeletedBook(file > 0))
        } else if let Some(cookies) = num("imported") {
            Some(Self::SessionImported { cookies })
        } else if num("cleared").is_some() {
            Some(Self::SessionCleared)
        } else {
            num("retried").map(|retried| Self::Bulk {
                retried,
                skipped: num("skipped").unwrap_or(0),
            })
        }
    }
}

// ----------------------------------------------------------------------- layout

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Nav {
    Dashboard,
    Library,
    History,
    Session,
    None,
}

fn pill(site: SiteStatus, now: Timestamp) -> Markup {
    let (class, text) = site_summary(site, now);
    html! { div id="site" data-pill class=(format!("pill {class}")) role="status" { (text) } }
}

/// What a live update returns: the site pill, which the script moves into place,
/// followed by the page content.
/// The row above the page content: the site status, and on the dashboard the add form
/// beside it on a wide screen.
fn bar(site: SiteStatus, now: Timestamp, extra: Option<Markup>) -> Markup {
    html! { div class="bar" { (pill(site, now)) @if let Some(extra) = extra { (extra) } } }
}

fn live(site: SiteStatus, now: Timestamp, content: &Markup) -> Markup {
    html! { (pill(site, now)) (content) }
}

fn notice_view(notice: Notice) -> Markup {
    let (class, text, job) = match notice {
        Notice::Added(id) => ("", "Added to the queue.", Some(id)),
        Notice::AlreadyQueued(id) => (
            "warn",
            "That work is already queued or downloading.",
            Some(id),
        ),
        Notice::Retried(id) => ("", "Queued again.", Some(id)),
        Notice::ForceRetried(id) => ("", "Queued to download again from scratch.", Some(id)),
        Notice::Cancelled(id) => ("warn", "Cancelled.", Some(id)),
        Notice::DeletedBook(true) => (
            "warn",
            "The book was deleted from the library. Its history is kept.",
            None,
        ),
        Notice::DeletedBook(false) => ("warn", "There was no book to delete.", None),
        Notice::Bulk { .. } | Notice::SessionImported { .. } | Notice::DeletedSeries { .. } => {
            ("", "", None)
        }
        Notice::SessionCleared => ("warn", "The browser session was forgotten.", None),
    };
    html! {
        p class=(format!("notice {class}")) role="status" {
            @if let Notice::Bulk { retried, skipped } = notice {
                "Queued " (retried) " again."
                @if skipped > 0 { " Skipped " (skipped) " that could not be retried (already queued, or changed since you looked)." }
            } @else if let Notice::SessionImported { cookies } = notice {
                "Imported " (cookies) " cookies. They are used from the next download."
            } @else if let Notice::DeletedSeries { deleted, kept } = notice {
                "The series is no longer followed. Deleted " (deleted) (if deleted == 1 { " book" } else { " books" }) "."
                @if kept > 0 { " Kept " (kept) " that also belong to another series you follow." }
            } @else {
                (text)
                @if let Some(id) = job { " " a href=(format!("/jobs/{id}")) { "View job" } }
            }
        }
    }
}

fn nav_link(label: &str, href: &str, here: bool) -> Markup {
    html! { a href=(href) aria-current=[here.then_some("page")] { (label) } }
}

/// Everything a full page needs besides its live content.
#[derive(Debug, Clone, Copy)]
pub struct Frame<'a> {
    pub title: &'a str,
    pub nav: Nav,
    /// Set on a job page: live events for other jobs are ignored there.
    pub job: Option<JobId>,
    pub notice: Option<Notice>,
}

/// A complete page. `above` sits outside the live region (so a refresh never
/// disturbs it); `content` is the live region's body, already containing the pill.
#[must_use]
pub fn page(frame: Frame<'_>, above: &Markup, content: &Markup) -> Markup {
    let Frame {
        title,
        nav,
        job,
        notice,
    } = frame;
    html! {
        (DOCTYPE)
        html lang="en" data-live="off" {
            head {
                meta charset="utf-8";
                meta name="viewport" content="width=device-width, initial-scale=1, viewport-fit=cover";
                meta name="color-scheme" content="light dark";
                meta name="theme-color" content="#eef1f5" media="(prefers-color-scheme: light)";
                meta name="theme-color" content="#0f151c" media="(prefers-color-scheme: dark)";
                title { (title) " · Fanfold" }
                link rel="icon" type="image/svg+xml" href="/static/icon.svg";
                link rel="stylesheet" href="/static/app.css";
                script src="/static/app.js" defer {}
            }
            body {
                header class="top" {
                    div class="wrap" {
                        a class="brand" href="/" { "Fanfold" }
                        nav class="nav" aria-label="Main" {
                            (nav_link("Dashboard", "/", nav == Nav::Dashboard))
                            (nav_link("Library", "/library", nav == Nav::Library))
                            (nav_link("History", "/jobs", nav == Nav::History))
                            (nav_link("Session", "/session", nav == Nav::Session))
                        }
                        span class="livedot" title="Live updates" {}
                    }
                }
                main class="wrap" {
                    @if let Some(notice) = notice { (notice_view(notice)) }
                    (above)
                    div id="live" data-job=[job.map(|j| j.to_string())] { (content) }
                }
            }
        }
    }
}

// -------------------------------------------------------------------- job cards

fn when(now: Timestamp, ts: Timestamp) -> Markup {
    html! { time datetime=(iso(ts)) title=(iso(ts)) { (relative(now, ts)) } }
}

/// One tappable card. The whole card is the link.
fn card(row: &JobRow, now: Timestamp) -> Markup {
    let status = status_of(&row.job);
    let job = &row.job;
    html! {
        a class=(format!("job {}", status.class)) href=(format!("/jobs/{}", job.id)) {
            span class="ribbon" aria-hidden="true" {}
            span class="body" {
                span class="title" { (row.name()) }
                span class="meta" {
                    span class="chip" { (status.label) }
                    @if row.is_series() { span class="chip series" { "Series" } }
                    @if row.title.is_some() { span class="mono" { (row.id_label()) } }
                    @if job.attempt > 0 { span { "tried " (job.attempt) "×" } }
                    @match job.state {
                        JobState::Queued if job.last_error.is_some() => { span { "next try " (when(now, job.next_attempt_at)) } }
                        _ => { span { (when(now, job.updated_at)) } }
                    }
                }
                @if let Some(error) = job.last_error.as_deref().filter(|_| job.state != JobState::Succeeded) {
                    span class="err" { (truncate(first_line(error), 200)) }
                }
            }
        }
    }
}

fn card_list(rows: &[JobRow], now: Timestamp) -> Markup {
    html! { ul class="jobs" { @for row in rows { li { (card(row, now)) } } } }
}

// -------------------------------------------------------------------- dashboard

#[must_use]
pub fn dashboard_content(m: &DashboardModel) -> Markup {
    html! {
        section {
            h2 { "Last " (m.shelf.len()) " results" }
            @if m.shelf.is_empty() {
                p class="empty" { "Nothing has finished yet. Paste an AO3 link above to get started." }
            } @else {
                ul class="shelf" aria-label="Recent results, oldest first" {
                    @for row in &m.shelf {
                        @let status = status_of(&row.job);
                        li {
                            a class=(status.class) href=(format!("/jobs/{}", row.job.id))
                              title=(format!("{}: {}", row.name(), status.label))
                              aria-label=(format!("{}: {}", row.name(), status.label)) {}
                        }
                    }
                }
                p class="shelf-note" { "Oldest on the left, newest on the right." }
            }
        }
        // One column on a phone; on a wide screen the work queue sits beside the history.
        div class="board" {
            div class="board-main" {
                section {
                    h2 { "Needs attention" @if !m.attention.is_empty() { span class="count" { (m.attention.len()) } } }
                    @if m.attention.is_empty() {
                        p class="empty" { "Nothing needs attention." }
                    } @else {
                        (card_list(&m.attention, m.now))
                        @if m.counts.failed > 0 {
                            p { a href="/jobs?filter=failed" { "All " (m.counts.failed) " failed jobs →" } }
                        }
                    }
                }
                section {
                    h2 { "In progress" @if !m.in_progress.is_empty() { span class="count" { (m.in_progress.len()) } } }
                    @if m.in_progress.is_empty() {
                        p class="empty" { "No downloads are running or waiting." }
                    } @else {
                        (card_list(&m.in_progress, m.now))
                    }
                }
            }
            div class="board-side" {
                section {
                    h2 { "Recent" a class="more" href="/jobs" { "All history →" } }
                    @if m.recent.is_empty() {
                        p class="empty" { "No downloads yet." }
                    } @else {
                        (card_list(&m.recent, m.now))
                    }
                }
            }
        }
    }
}

/// The add-a-link form. It lives outside the live region so typing is never
/// interrupted by a refresh.
#[must_use]
pub fn add_form() -> Markup {
    html! {
        section class="add" {
            form method="post" action="/jobs" {
                label for="url" { "Add a work or a series" }
                div class="field" {
                    input id="url" name="url" type="url" required inputmode="url" autocomplete="off"
                          autocapitalize="off" spellcheck="false" placeholder="https://archiveofourown.org/works/… or /series/…";
                    button class="btn primary" type="submit" { "Download" }
                }
            }
        }
    }
}

/// The dashboard as a live update.
#[must_use]
pub fn dashboard(m: &DashboardModel) -> Markup {
    live(m.site, m.now, &dashboard_content(m))
}

/// History as a live update.
#[must_use]
pub fn history(m: &HistoryModel) -> Markup {
    live(m.site, m.now, &history_content(m))
}

/// A job page as a live update.
#[must_use]
pub fn job_detail(m: &JobModel) -> Markup {
    live(m.site, m.now, &job_content(m))
}

/// Dashboard as a full page.
#[must_use]
pub fn dashboard_page(m: &DashboardModel, notice: Option<Notice>) -> Markup {
    let frame = Frame {
        title: "Dashboard",
        nav: Nav::Dashboard,
        job: None,
        notice,
    };
    page(
        frame,
        &bar(m.site, m.now, Some(add_form())),
        &dashboard_content(m),
    )
}

// ---------------------------------------------------------------------- history

const FILTERS: [(JobFilter, &str); 5] = [
    (JobFilter::All, "All"),
    (JobFilter::Active, "Active"),
    (JobFilter::Retrying, "Retrying"),
    (JobFilter::Failed, "Failed"),
    (JobFilter::Succeeded, "Done"),
];

fn history_href(filter: JobFilter, page: u32) -> String {
    match (filter == JobFilter::All, page > 1) {
        (true, false) => "/jobs".to_owned(),
        (true, true) => format!("/jobs?page={page}"),
        (false, false) => format!("/jobs?filter={}", filter.as_str()),
        (false, true) => format!("/jobs?filter={}&page={page}", filter.as_str()),
    }
}

const fn is_retryable(job: &Job) -> bool {
    matches!(job.state, JobState::Failed | JobState::Cancelled)
}

/// Whether the job can be re-run from scratch, ignoring the stored book.
const fn can_force(job: &Job) -> bool {
    matches!(
        job.state,
        JobState::Succeeded | JobState::Failed | JobState::Cancelled
    )
}

/// One history row. A stacked card on a phone; on a wide screen the same cells line up
/// as table columns under the header row.
fn history_row(row: &JobRow, now: Timestamp) -> Markup {
    let status = status_of(&row.job);
    let job = &row.job;
    html! {
        a class=(format!("job {}", status.class)) href=(format!("/jobs/{}", job.id)) {
            span class="ribbon" aria-hidden="true" {}
            span class="body cells" {
                span class="c-title" { span class="title" { (row.name()) } }
                span class="c-status" { span class="chip" { (status.label) } }
                @if row.title.is_some() || row.is_series() { span class="c-id mono" { (row.id_label()) } }
                @if job.attempt > 0 { span class="c-tries" { "tried " (job.attempt) "×" } }
                span class="c-when" {
                    @if job.state == JobState::Queued && job.last_error.is_some() { "next try " }
                    (when(now, if job.state == JobState::Queued && job.last_error.is_some() { job.next_attempt_at } else { job.updated_at }))
                }
                @if let Some(error) = job.last_error.as_deref().filter(|_| job.state != JobState::Succeeded) {
                    span class="c-err err" { (truncate(first_line(error), 200)) }
                }
            }
        }
    }
}

#[must_use]
pub fn history_content(m: &HistoryModel) -> Markup {
    let any_retryable = m.rows.iter().any(|r| is_retryable(&r.job));
    let count_for = |f: JobFilter| match f {
        JobFilter::Active => Some(m.counts.queued + m.counts.running + m.counts.retrying),
        JobFilter::Retrying => Some(m.counts.retrying),
        JobFilter::Failed => Some(m.counts.failed),
        JobFilter::Succeeded => Some(m.counts.succeeded),
        _ => None,
    };
    html! {
        h1 { "History" }
        section {
            ul class="filters" aria-label="Filter" {
                @for (filter, label) in FILTERS {
                    li {
                        a href=(history_href(filter, 1)) aria-current=[(filter == m.filter).then_some("page")] {
                            (label)
                            @if let Some(n) = count_for(filter) { " " span class="mono" { (n) } }
                        }
                    }
                }
            }
            @if m.rows.is_empty() {
                p class="empty" { "No jobs match this filter." }
            } @else {
                form method="post" action="/jobs/retry" {
                    // Column headers only appear on wide screens, where rows become a table.
                    div class="thead" aria-hidden="true" {
                        span { "Status" } span { "Work" } span { "ID" } span { "Tries" } span { "Updated" } span { "Last error" }
                    }
                    ul class="jobs hist" {
                        @for row in &m.rows {
                            @let pick = is_retryable(&row.job);
                            li class="row" {
                                @if pick {
                                    label class="pick" {
                                        input type="checkbox" name="id" value=(row.job.id.get());
                                        span class="sr-only" { "Select " (row.name()) }
                                    }
                                } @else {
                                    // Keeps the columns aligned on wide screens; hidden on phones.
                                    span class="pick-spacer" aria-hidden="true" {}
                                }
                                (history_row(row, m.now))
                            }
                        }
                    }
                    @if any_retryable || m.counts.failed > 0 {
                        div class="bulkbar" {
                            @if any_retryable { button class="btn primary" type="submit" { "Retry selected" } }
                            @if m.counts.failed > 0 {
                                button class="btn" type="submit" formaction="/jobs/retry-failed" {
                                    "Retry all " (m.counts.failed) " failed"
                                }
                            }
                        }
                    }
                }
                div class="pager" {
                    @if m.page > 1 { a class="btn" href=(history_href(m.filter, m.page - 1)) { "← Newer" } } @else { span {} }
                    @if m.has_next { a class="btn" href=(history_href(m.filter, m.page + 1)) { "Older →" } }
                }
            }
        }
    }
}

#[must_use]
pub fn history_page(m: &HistoryModel, notice: Option<Notice>) -> Markup {
    let frame = Frame {
        title: "History",
        nav: Nav::History,
        job: None,
        notice,
    };
    page(frame, &bar(m.site, m.now, None), &history_content(m))
}

// ------------------------------------------------------------------- job detail

const fn source_label(source: JobSource) -> &'static str {
    match source {
        JobSource::Mail => "AO3 email",
        JobSource::Manual => "Added by hand",
    }
}

const fn level_class(level: LogLevel) -> &'static str {
    match level {
        LogLevel::Info => "lvl-info",
        LogLevel::Warn => "lvl-warn",
        LogLevel::Error => "lvl-error",
    }
}

/// Split a job's log into runs: a new run starts at each "starting attempt" line.
#[must_use]
pub fn runs(logs: &[LogEntry]) -> Vec<&[LogEntry]> {
    let mut starts: Vec<usize> = logs
        .iter()
        .enumerate()
        .filter(|(_, l)| l.message.starts_with(ATTEMPT_START_PREFIX))
        .map(|(i, _)| i)
        .collect();
    if starts.first() != Some(&0) && !logs.is_empty() {
        starts.insert(0, 0);
    }
    starts
        .iter()
        .enumerate()
        .filter_map(|(n, start)| logs.get(*start..starts.get(n + 1).copied().unwrap_or(logs.len())))
        .collect()
}

#[must_use]
pub fn job_content(m: &JobModel) -> Markup {
    let job = &m.row.job;
    let status = status_of(job);
    let log_runs = runs(&m.logs);
    let content = html! {
        a class="back" href="/jobs" { "← All jobs" }
        div class="jobgrid" {
        div class="jobside" {
        div class=(format!("head {}", status.class)) {
            span class="ribbon" aria-hidden="true" {}
            div class="body" {
                h1 { (m.row.name()) }
                p class="sub" {
                    span class="chip" { (status.label) } " "
                    @if m.row.title.is_some() {
                        span class="mono" { (if m.row.is_series() { "" } else { "work " }) (m.row.id_label()) } " · "
                    }
                    a href=(job.target.canonical_url()) rel="noopener noreferrer" { "Open on AO3" }
                }
                dl class="facts" {
                    div { dt { "Source" } dd { (source_label(job.source)) } }
                    div { dt { "Failed attempts" } dd { (job.attempt) " of " (m.max_attempts) } }
                    @if job.state == JobState::Queued {
                        div { dt { "Next try" } dd { (when(m.now, job.next_attempt_at)) } }
                    }
                    @if job.force && job.state.is_active() {
                        div { dt { "Download" } dd { "from scratch" } }
                    }
                    div { dt { "Updated" } dd { (when(m.now, job.updated_at)) } }
                    div { dt { "Added" } dd { (when(m.now, job.created_at)) } }
                    @if let Some(book) = &m.book { div { dt { "File" } dd class="mono" { (book.file_name) } } }
                }
                @if let Some(error) = job.last_error.as_deref().filter(|_| job.state != JobState::Succeeded) {
                    p class="err" { (truncate(first_line(error), 400)) }
                }
                @if can_force(job) || job.state == JobState::Queued {
                    div class="actions" {
                        @if is_retryable(job) {
                            form method="post" action=(format!("/jobs/{}/retry", job.id)) {
                                button class="btn primary" type="submit" { "Retry" }
                            }
                        }
                        @if can_force(job) {
                            form method="post" action=(format!("/jobs/{}/force-retry", job.id)) {
                                @if m.row.is_series() {
                                    button class="btn" type="submit"
                                        title="List the series again and queue every work in it that is not queued already." {
                                        "List the series again"
                                    }
                                } @else {
                                    button class="btn" type="submit"
                                        title="Ignore the stored book and download the whole work again. The file is replaced only if the download succeeds." {
                                        "Re-download from scratch"
                                    }
                                }
                            }
                        }
                        @if job.state == JobState::Queued {
                            form method="post" action=(format!("/jobs/{}/cancel", job.id)) {
                                button class="btn danger" type="submit" { "Cancel" }
                            }
                        }
                    }
                }
            }
        }
        }
        (delete_controls(m))
        section {
            h2 { "Log" @if log_runs.len() > 1 { " " span class="hint" { "newest first" } } }
            @if log_runs.is_empty() {
                p class="empty" { "No log lines yet." }
            } @else {
                div class="log" tabindex="0" role="log" aria-label="Job log" {
                    // Newest run first; lines within a run stay in order.
                    @for (n, run) in log_runs.iter().enumerate().rev() {
                        div class="run" {
                            h3 { "Run " (n + 1) @if let Some(first) = run.first() { " · " (when(m.now, first.at)) } }
                            @for line in *run {
                                div class="line" {
                                    time datetime=(iso(line.at)) title=(iso(line.at)) { (clock(line.at)) }
                                    span class=(format!("lvl {}", level_class(line.level))) { (line.level.as_str().to_uppercase()) }
                                    pre { (line.message) }
                                }
                            }
                        }
                    }
                }
            }
        }
        }
    };
    content
}

/// "Delete…": a book that is in the library, or a series. Folded away, so a single slip of
/// the finger cannot delete anything.
fn delete_controls(m: &JobModel) -> Markup {
    let job = &m.row.job;
    let (action, button, explain) = match job.target {
        Target::Work(work) if m.book.is_some() => (
            format!("/works/{work}/delete"),
            "Delete the book",
            "Removes the EPUB from your library and cancels queued downloads of it. Its history is kept; a new download (or an AO3 email) brings it back.",
        ),
        Target::Series(series) => (
            format!("/series/{series}/delete"),
            "Delete the series and its books",
            "Stops following the series, cancels its queued downloads and removes its books, except those that also belong to another series you follow.",
        ),
        Target::Work(_) => return html! {},
    };
    html! {
        details class="danger" {
            summary { "Delete…" }
            p { (explain) }
            form method="post" action=(action) {
                input type="hidden" name="job" value=(job.id);
                button class="btn danger" type="submit" { (button) }
            }
        }
    }
}

#[must_use]
pub fn job_page(m: &JobModel, notice: Option<Notice>) -> Markup {
    let name = m.row.name();
    let frame = Frame {
        title: &name,
        nav: Nav::None,
        job: Some(m.row.job.id),
        notice,
    };
    page(frame, &bar(m.site, m.now, None), &job_content(m))
}

// ---------------------------------------------------------------------- session

#[derive(Debug)]
pub struct SessionModel {
    pub now: Timestamp,
    pub site: SiteStatus,
    /// The site the cookies are for, shown in the help text.
    pub cookie_site: String,
    pub session: Option<SessionView>,
}

const fn cookie_status(status: CookieStatus) -> (&'static str, &'static str) {
    match status {
        CookieStatus::Session => ("cancelled", "Session"),
        CookieStatus::Valid => ("done", "Valid"),
        CookieStatus::ExpiringSoon => ("retrying", "Expiring soon"),
        CookieStatus::Expired => ("failed", "Expired"),
    }
}

/// The stored session, without a single cookie value.
#[must_use]
pub fn session_content(m: &SessionModel) -> Markup {
    html! {
        h1 { "Browser session" }
        @match &m.session {
            None => {
                p class="muted" {
                    "No session stored. Downloads run without cookies, so a site that checks for a browser "
                    "(Cloudflare) may refuse or slow them down."
                }
            }
            Some(view) => {
                @let expired = view.cookies.iter().filter(|c| c.status == CookieStatus::Expired).count();
                @let soon = view.cookies.iter().filter(|c| c.status == CookieStatus::ExpiringSoon).count();
                @if expired > 0 || soon > 0 {
                    p class="notice warn" role="status" {
                        @if expired > 0 { (expired) " expired (no longer sent). " }
                        @if soon > 0 { (soon) " expiring soon. " }
                        "Import fresh cookies below to keep downloads working."
                    }
                }
                p {
                    "Sent as " code { (view.user_agent) } ". Updated " (when(m.now, view.updated_at)) "."
                    @if let Some(next) = view.next_expiry {
                        " Next expiry " (when(m.now, next)) "."
                    }
                }
                div class="tablewrap" {
                    table class="cookies" {
                        thead { tr { th { "Cookie" } th { "Domain" } th { "Expires" } th { "State" } } }
                        tbody {
                            @for c in &view.cookies {
                                @let (class, label) = cookie_status(c.status);
                                tr {
                                    td { code { (c.name) } @if c.http_only { " " small class="muted" { "HttpOnly" } } }
                                    td { (c.domain) }
                                    td {
                                        @if let Some(at) = c.expires { (when(m.now, at)) } @else { span class="muted" { "when the session ends" } }
                                    }
                                    td { span class=(format!("chip {class}")) { (label) } }
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}

/// The import and clear forms. Outside the live region so a refresh never wipes
/// what is being pasted.
#[must_use]
pub fn session_forms(m: &SessionModel) -> Markup {
    html! {
        section class="add session-form" {
            form method="post" action="/session" {
                label for="cookies" { "Import cookies" }
                p class="muted" {
                    "Paste a " code { "cookies.txt" } " (Netscape format) or a HAR file saved from your "
                    "browser's network tab for " (m.cookie_site) ". A HAR also supplies the exact "
                    "User-Agent, which Cloudflare ties its clearance cookie to. Imports are merged "
                    "with what is stored: paste both to combine them. Values are never shown again."
                }
                textarea id="cookies" name="cookies" rows="8" required spellcheck="false"
                         autocomplete="off" autocapitalize="off" placeholder="# Netscape HTTP Cookie File …  or  {\"log\": …" {}
                label for="user_agent" { "User-Agent " span class="muted" { "(optional with a HAR)" } }
                input id="user_agent" name="user_agent" type="text" autocomplete="off" spellcheck="false"
                      autocapitalize="off" placeholder="Mozilla/5.0 (X11; Linux x86_64; rv:…) Gecko/… Firefox/…";
                div class="field" {
                    button class="btn primary" type="submit" { "Import" }
                }
            }
            @if m.session.is_some() {
                form method="post" action="/session/clear" {
                    button class="btn danger" type="submit" { "Forget the session" }
                }
            }
        }
    }
}

/// The session page as a live update.
#[must_use]
pub fn session_detail(m: &SessionModel) -> Markup {
    live(m.site, m.now, &session_content(m))
}

#[must_use]
pub fn session_page(m: &SessionModel, notice: Option<Notice>) -> Markup {
    let frame = Frame {
        title: "Session",
        nav: Nav::Session,
        job: None,
        notice,
    };
    page(
        frame,
        &html! { (bar(m.site, m.now, None)) (session_forms(m)) },
        &session_content(m),
    )
}

// ----------------------------------------------------------------------- errors

#[must_use]
pub fn error_page(problem: &ProblemDetails) -> Markup {
    let content = html! {
        h1 { (problem.title) }
        @if let Some(detail) = &problem.detail { p { (detail) } }
        p { a class="btn" href="/" { "Back to the dashboard" } }
    };
    let frame = Frame {
        title: &problem.title,
        nav: Nav::None,
        job: None,
        notice: None,
    };
    page(frame, &html! {}, &content)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(secs: i64) -> Timestamp {
        Timestamp::from_second(secs).unwrap()
    }

    fn entry(secs: i64, message: &str) -> LogEntry {
        LogEntry {
            at: t(secs),
            attempt: 0,
            level: LogLevel::Info,
            message: message.to_owned(),
        }
    }

    fn pairs(query: &str) -> Vec<(String, String)> {
        crate::forms::pairs(query.as_bytes())
    }

    #[test]
    fn logs_split_into_runs_at_each_attempt_start() {
        let logs = [
            entry(1, "starting attempt 1"),
            entry(2, "failed (site_unavailable): 503"),
            entry(3, "starting attempt 1"),
            entry(4, "downloaded \"X\""),
        ];
        let runs = runs(&logs);
        assert_eq!(runs.len(), 2);
        assert_eq!(runs[0].len(), 2);
        assert_eq!(runs[1][1].message, "downloaded \"X\"");
    }

    #[test]
    fn leading_lines_without_a_start_marker_still_form_a_run() {
        let logs = [
            entry(1, "orphan line"),
            entry(2, "starting attempt 1"),
            entry(3, "x"),
        ];
        let runs = runs(&logs);
        assert_eq!((runs.len(), runs[0].len(), runs[1].len()), (2, 1, 2));
        assert_eq!(super::runs(&[]), Vec::<&[LogEntry]>::new());
    }

    #[test]
    fn notices_are_built_from_numbers_only() {
        assert_eq!(
            Notice::from_query(&pairs("added=12")),
            Some(Notice::Added(JobId::new(12)))
        );
        assert_eq!(
            Notice::from_query(&pairs("queued=3")),
            Some(Notice::AlreadyQueued(JobId::new(3)))
        );
        assert_eq!(
            Notice::from_query(&pairs("retried_one=4")),
            Some(Notice::Retried(JobId::new(4)))
        );
        assert_eq!(
            Notice::from_query(&pairs("forced=6")),
            Some(Notice::ForceRetried(JobId::new(6)))
        );
        assert_eq!(
            Notice::from_query(&pairs("cancelled=5")),
            Some(Notice::Cancelled(JobId::new(5)))
        );
        assert_eq!(
            Notice::from_query(&pairs("filter=failed&retried=3&skipped=1")),
            Some(Notice::Bulk {
                retried: 3,
                skipped: 1
            })
        );
        assert_eq!(
            Notice::from_query(&pairs("retried=2")),
            Some(Notice::Bulk {
                retried: 2,
                skipped: 0
            })
        );
        for hostile in [
            "added=<script>",
            "added=-1",
            "added=0",
            "added=",
            "added=1.5",
            "x=1",
            "",
        ] {
            assert_eq!(Notice::from_query(&pairs(hostile)), None, "{hostile:?}");
        }
    }

    #[test]
    fn history_links_keep_the_filter_and_page() {
        assert_eq!(history_href(JobFilter::All, 1), "/jobs");
        assert_eq!(history_href(JobFilter::All, 3), "/jobs?page=3");
        assert_eq!(history_href(JobFilter::Failed, 1), "/jobs?filter=failed");
        assert_eq!(
            history_href(JobFilter::Failed, 2),
            "/jobs?filter=failed&page=2"
        );
    }
}
