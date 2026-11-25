//! HTML, rendered on the server with `maud`.
//!
//! Everything interpolated with `(expr)` is HTML-escaped. Titles, log lines and error
//! messages all originate from a third-party site, so that is not optional. There are
//! no inline styles, scripts or event handlers: the CSP forbids them.

use fanfold_app::{
    ports::{BookRecord, JobCounts, JobFilter, LogEntry, LogLevel, SiteStatus},
    worker::ATTEMPT_START_PREFIX,
};
use fanfold_domain::{
    Timestamp,
    job::{Job, JobId, JobSource, JobState, Target},
};
use maud::{DOCTYPE, Markup, html};

use crate::{
    assets,
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
    #[must_use]
    pub fn name(&self) -> String {
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
    None,
}

/// The colour of a job's status (`format::Status::class`).
const fn tone(status_class: &str) -> &'static str {
    match status_class.as_bytes() {
        b"done" | b"resolved" => "tone-ok",
        b"retrying" => "tone-warn",
        b"failed" => "tone-err",
        b"running" => "tone-run",
        b"queued" => "tone-info",
        _ => "tone-muted",
    }
}

fn pill(site: SiteStatus, now: Timestamp) -> Markup {
    let (class, text) = site_summary(site, now);
    let tone = match class {
        "ok" => "tone-ok",
        "warn" => "tone-warn",
        _ => "tone-err",
    };
    html! { div id="site" class=(format!("chip chip-dot {tone} pill {class}")) role="status" { (text) } }
}

/// The row above the page content: the site status, and on the dashboard the add form.
fn bar(site: SiteStatus, now: Timestamp, extra: Option<Markup>) -> Markup {
    html! {
        div class="flex flex-col gap-4 sm:flex-row sm:items-center sm:justify-between" {
            div { (pill(site, now)) }
            @if let Some(extra) = extra { (extra) }
        }
    }
}

fn notice_view(notice: Notice) -> Markup {
    let (tone, text, job) = match notice {
        Notice::Added(id) => ("tone-ok", "Added to the queue.", Some(id)),
        Notice::AlreadyQueued(id) => (
            "tone-warn",
            "That work is already queued or downloading.",
            Some(id),
        ),
        Notice::Retried(id) => ("tone-ok", "Queued again.", Some(id)),
        Notice::ForceRetried(id) => (
            "tone-ok",
            "Queued to download again from scratch.",
            Some(id),
        ),
        Notice::Cancelled(id) => ("tone-warn", "Cancelled.", Some(id)),
        Notice::DeletedBook(true) => (
            "tone-warn",
            "The book was deleted from the library. Its history is kept.",
            None,
        ),
        Notice::DeletedBook(false) => ("tone-warn", "There was no book to delete.", None),
        Notice::Bulk { .. } | Notice::DeletedSeries { .. } => ("tone-ok", "", None),
    };
    html! {
        p class=(format!("notice {tone} rounded-lg border border-[color-mix(in_oklab,var(--tone)_35%,transparent)] bg-[color-mix(in_oklab,var(--tone)_8%,transparent)] px-4 py-2.5 text-sm")) role="status" {
            @if let Notice::Bulk { retried, skipped } = notice {
                "Queued " (retried) " again."
                @if skipped > 0 { " Skipped " (skipped) " that could not be retried (already queued, or changed since you looked)." }
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
    html! {
        a href=(href)
          class="rounded-md px-2.5 py-1.5 text-sm font-medium text-muted no-underline hover:bg-bar hover:text-ink hover:no-underline aria-[current=page]:bg-bar aria-[current=page]:text-ink"
          aria-current=[here.then_some("page")] { (label) }
    }
}

/// What the live region listens for: every change, or one job's.
fn live_trigger(job: Option<JobId>) -> String {
    let changes = job.map_or_else(
        || "refresh from:body delay:200ms".to_owned(),
        |id| format!("job-{id} from:body delay:200ms"),
    );
    // A reconnect may have missed events; the timer keeps "5 min ago" honest.
    format!(
        "{changes}, resync from:body, htmx:sse:after:connection from:body delay:200ms, every 30s"
    )
}

/// One page, as the router serves it: whole, or only its live region (a live update).
#[derive(Debug)]
pub struct Screen<'a> {
    pub title: &'a str,
    pub nav: Nav,
    /// Path and query the live region refreshes from: this page's own URL.
    pub url: &'a str,
    /// Set on a job page: only that job's changes refresh it.
    pub job: Option<JobId>,
    pub site: SiteStatus,
    pub now: Timestamp,
    /// Above the live region (never disturbed by a refresh).
    pub above: Markup,
    /// The live region's content.
    pub content: Markup,
    /// Other parts of the page a live update replaces too: (target selector, content).
    pub partials: Vec<(&'static str, Markup)>,
}

/// The live region: re-fetched (and morphed in place, so focus, scroll positions and
/// selections survive) whenever the event stream says something changed.
///
/// A refresh fetches the address the region was rendered for, so it gives way (`drop`) to
/// any request already in flight on it: a search (which syncs on `#live` with `replace`)
/// would otherwise be cancelled and the old results morphed back under the new query. A
/// dropped refresh is caught up by the next event or the timer.
fn live_region(screen: &Screen<'_>) -> Markup {
    if screen.url.is_empty() {
        return html! { div id="live" class="flex flex-col gap-6" { (screen.content) } };
    }
    html! {
        div id="live" hx-get=(screen.url) hx-trigger=(live_trigger(screen.job)) hx-swap="outerMorph"
            hx-sync="this:drop" hx-status:4xx="swap:none" hx-status:5xx="swap:none"
            data-job=[screen.job.map(|j| j.to_string())] class="flex flex-col gap-6" {
            (screen.content)
        }
    }
}

/// What a live update returns: the live region, and the parts it replaces elsewhere.
#[must_use]
pub fn fragment(screen: &Screen<'_>) -> Markup {
    html! {
        (live_region(screen))
        hx-partial hx-target="#site" hx-swap="outerHTML" { (pill(screen.site, screen.now)) }
        @for (target, content) in &screen.partials {
            hx-partial hx-target=(target) hx-swap="innerHTML" { (content) }
        }
    }
}

/// htmx's settings: its own indicator stylesheet off (the CSP allows no inline styles; ours
/// is in app.css), only the extensions we ship, and Back/Forward reload the page (every
/// page renders whole from its URL).
const HTMX_CONFIG: &str = r#"{"includeIndicatorCSS":false,"extensions":"sse, alpine-compat","defaultSwap":"outerMorph","history":"reload"}"#;

/// A complete page.
#[must_use]
pub fn full_page(screen: &Screen<'_>, notice: Option<Notice>) -> Markup {
    html! {
        (DOCTYPE)
        html lang="en" data-live="off" {
            head {
                meta charset="utf-8";
                meta name="viewport" content="width=device-width, initial-scale=1, viewport-fit=cover";
                meta name="color-scheme" content="light dark";
                meta name="theme-color" content="#f3f6f3" media="(prefers-color-scheme: light)";
                meta name="theme-color" content="#101517" media="(prefers-color-scheme: dark)";
                meta name="htmx-config" content=(HTMX_CONFIG);
                title { (screen.title) " · Fanfold" }
                // Content-hashed addresses (`crate::assets`): cached for good, renamed by
                // any release that changes them.
                link rel="icon" type="image/svg+xml" href=(assets::url("icon.svg"));
                link rel="preload" href=(assets::url("vendor/literata-latin.woff2")) as="font" type="font/woff2" crossorigin;
                link rel="stylesheet" href=(assets::url("app.css"));
                // In this order: htmx, its extensions, our components (registered before
                // Alpine starts), Alpine.
                script src=(assets::url("vendor/htmx.min.js")) defer {}
                script src=(assets::url("vendor/hx-sse.min.js")) defer {}
                script src=(assets::url("vendor/hx-alpine-compat.min.js")) defer {}
                script src=(assets::url("app.js")) defer {}
                script src=(assets::url("vendor/alpine-csp.min.js")) defer {}
            }
            body hx-sse:connect="/events" {
                header class="sticky top-0 z-30 border-b border-rule bg-paper/85 backdrop-blur" {
                    div class="wrap flex h-14 items-center gap-3" {
                        a class="display text-xl font-semibold italic text-ink no-underline hover:no-underline" href="/" { "Fanfold" }
                        span class="livedot" title="Live updates" {}
                        nav class="ml-auto flex gap-0.5" aria-label="Main" {
                            (nav_link("Dashboard", "/", screen.nav == Nav::Dashboard))
                            (nav_link("Library", "/library", screen.nav == Nav::Library))
                            (nav_link("History", "/jobs", screen.nav == Nav::History))
                        }
                    }
                }
                main class="wrap flex flex-col gap-6 py-6 pb-16" {
                    @if let Some(notice) = notice { (notice_view(notice)) }
                    (screen.above)
                    (live_region(screen))
                }
            }
        }
    }
}

// -------------------------------------------------------------------- job cards

fn when(now: Timestamp, ts: Timestamp) -> Markup {
    html! { time datetime=(iso(ts)) title=(iso(ts)) { (relative(now, ts)) } }
}

fn status_chip(job: &Job) -> Markup {
    let status = status_of(job);
    html! { span class=(format!("chip chip-dot {}", tone(status.class))) { (status.label) } }
}

/// One tappable card. The whole card is the link; its left edge carries the status colour.
fn card(row: &JobRow, now: Timestamp) -> Markup {
    let status = status_of(&row.job);
    let job = &row.job;
    html! {
        a class=(format!("job {} {} group flex gap-3 rounded-lg border border-rule bg-sheet p-3 text-ink no-underline transition-colors hover:border-ink/25 hover:no-underline", status.class, tone(status.class)))
          href=(format!("/jobs/{}", job.id)) {
            span class="w-1 shrink-0 self-stretch rounded-full bg-[var(--tone)]" aria-hidden="true" {}
            span class="flex min-w-0 flex-1 flex-col gap-1" {
                span class="title truncate font-medium group-hover:text-ribbon" { (row.name()) }
                span class="flex flex-wrap items-center gap-x-2.5 gap-y-1 text-xs text-muted" {
                    (status_chip(job))
                    @if row.is_series() { span class="chip tone-info" { "Series" } }
                    @if row.title.is_some() { span class="mono" { (row.id_label()) } }
                    @if job.attempt > 0 { span { "tried " (job.attempt) "×" } }
                    @match job.state {
                        JobState::Queued if job.last_error.is_some() => { span { "next try " (when(now, job.next_attempt_at)) } }
                        _ => { span { (when(now, job.updated_at)) } }
                    }
                }
                @if let Some(error) = job.last_error.as_deref().filter(|_| job.state != JobState::Succeeded) {
                    span class="err line-clamp-2 font-mono text-xs break-words text-err" { (truncate(first_line(error), 200)) }
                }
            }
        }
    }
}

fn card_list(rows: &[JobRow], now: Timestamp) -> Markup {
    html! { ul class="jobs flex flex-col gap-2" { @for row in rows { li { (card(row, now)) } } } }
}

fn section_head(label: &str, count: Option<usize>, extra: Option<Markup>) -> Markup {
    html! {
        div class="mb-3 flex items-baseline justify-between gap-3" {
            h2 class="eyebrow" { (label) @if let Some(n) = count.filter(|n| *n > 0) { span class="count" { (n) } } }
            @if let Some(extra) = extra { (extra) }
        }
    }
}

// -------------------------------------------------------------------- dashboard

#[must_use]
pub fn dashboard_content(m: &DashboardModel) -> Markup {
    html! {
        section aria-labelledby="results-h" {
            div class="mb-3 flex items-baseline justify-between gap-3" {
                h2 id="results-h" class="eyebrow" { "Last " (m.shelf.len()) " results" }
                @if !m.shelf.is_empty() { span class="shelf-note text-xs text-muted" { "Oldest on the left, newest on the right." } }
            }
            @if m.shelf.is_empty() {
                p class="empty" { "Nothing has finished yet. Paste an AO3 link above to get started." }
            } @else {
                ul class="shelf fanfold" aria-label="Recent results, oldest first" {
                    @for row in &m.shelf {
                        @let status = status_of(&row.job);
                        li {
                            a class=(format!("{} {}", status.class, tone(status.class))) href=(format!("/jobs/{}", row.job.id))
                              title=(format!("{}: {}", row.name(), status.label))
                              aria-label=(format!("{}: {}", row.name(), status.label)) {}
                        }
                    }
                }
            }
        }
        // One column on a phone; on a wide screen the work queue sits beside the history.
        div class="board grid grid-cols-1 gap-8 lg:grid-cols-[minmax(0,3fr)_minmax(0,2fr)]" {
            div class="board-main flex min-w-0 flex-col gap-8" {
                section {
                    (section_head("Needs attention", Some(m.attention.len()), None))
                    @if m.attention.is_empty() {
                        p class="empty" { "Nothing needs attention." }
                    } @else {
                        (card_list(&m.attention, m.now))
                        @if m.counts.failed > 0 {
                            p class="mt-3 text-sm" { a href="/jobs?filter=failed" { "All " (m.counts.failed) " failed jobs →" } }
                        }
                    }
                }
                section {
                    (section_head("In progress", Some(m.in_progress.len()), None))
                    @if m.in_progress.is_empty() {
                        p class="empty" { "No downloads are running or waiting." }
                    } @else {
                        (card_list(&m.in_progress, m.now))
                    }
                }
            }
            div class="board-side min-w-0" {
                section {
                    (section_head("Recent", None, Some(html! { a class="more text-sm" href="/jobs" { "All history →" } })))
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
        section class="add w-full sm:max-w-xl" {
            form method="post" action="/jobs" class="flex flex-col gap-1.5" {
                label for="url" class="eyebrow" { "Add a work or a series" }
                div class="field-row flex gap-2" {
                    input id="url" name="url" type="url" required inputmode="url" autocomplete="off"
                          autocapitalize="off" spellcheck="false" class="field font-mono text-sm"
                          placeholder="https://archiveofourown.org/works/… or /series/…";
                    button class="btn btn-primary shrink-0" type="submit" { "Download" }
                }
            }
        }
    }
}

#[must_use]
pub fn dashboard_screen<'a>(m: &DashboardModel, url: &'a str) -> Screen<'a> {
    Screen {
        title: "Dashboard",
        nav: Nav::Dashboard,
        url,
        job: None,
        site: m.site,
        now: m.now,
        above: bar(m.site, m.now, Some(add_form())),
        content: dashboard_content(m),
        partials: Vec::new(),
    }
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

/// Failed or cancelled; a failure a later job already fixed has nothing left to retry.
const fn is_retryable(job: &Job) -> bool {
    job.needs_attention() || matches!(job.state, JobState::Cancelled)
}

/// Whether the job can be re-run from scratch, ignoring the stored book.
const fn can_force(job: &Job) -> bool {
    matches!(
        job.state,
        JobState::Succeeded | JobState::Failed | JobState::Cancelled
    )
}

/// The columns of a history row on a wide screen.
const HISTORY_GRID: &str =
    "md:grid md:grid-cols-[1.75rem_7rem_minmax(0,1fr)_7rem_4rem_7rem] md:items-center md:gap-3";

/// One history row: a stacked card on a phone, table columns on a wide screen.
fn history_row(row: &JobRow, now: Timestamp) -> Markup {
    let status = status_of(&row.job);
    let job = &row.job;
    let next_try = job.state == JobState::Queued && job.last_error.is_some();
    html! {
        a class=(format!("job {} {} contents text-ink no-underline hover:no-underline", status.class, tone(status.class))) href=(format!("/jobs/{}", job.id)) {
            span class="c-status" { (status_chip(job)) }
            span class="c-title min-w-0" {
                span class="title block truncate font-medium hover:text-ribbon" { (row.name()) }
                @if let Some(error) = job.last_error.as_deref().filter(|_| job.state != JobState::Succeeded) {
                    span class="c-err err block truncate font-mono text-xs text-err" { (truncate(first_line(error), 200)) }
                }
            }
            span class="c-id mono text-muted" { @if row.title.is_some() || row.is_series() { (row.id_label()) } }
            span class="c-tries text-xs text-muted" { @if job.attempt > 0 { "tried " (job.attempt) "×" } }
            span class="c-when text-xs text-muted" {
                @if next_try { "next try " }
                (when(now, if next_try { job.next_attempt_at } else { job.updated_at }))
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
        div class="flex flex-wrap items-end justify-between gap-3" {
            h1 class="display text-3xl font-semibold" { "History" }
            nav class="filters seg" aria-label="Filter" {
                @for (filter, label) in FILTERS {
                    a href=(history_href(filter, 1)) aria-current=[(filter == m.filter).then_some("page")] {
                        (label)
                        @if let Some(n) = count_for(filter) { " " span class="mono tabular-nums opacity-70" { (n) } }
                    }
                }
            }
        }
        section {
            @if m.rows.is_empty() {
                p class="empty" { "No jobs match this filter." }
            } @else {
                form method="post" action="/jobs/retry" class="flex flex-col gap-3" {
                    div class="panel divide-y divide-rule overflow-hidden" {
                        // Column headers only appear on wide screens, where rows become a table.
                        div class="thead hidden px-3 py-2 eyebrow md:grid md:grid-cols-[1.75rem_7rem_minmax(0,1fr)_7rem_4rem_7rem] md:gap-3" aria-hidden="true" {
                            span {} span { "Status" } span { "Work" } span { "ID" } span { "Tries" } span { "Updated" }
                        }
                        ul class="jobs hist divide-y divide-rule" {
                            @for row in &m.rows {
                                @let pick = is_retryable(&row.job);
                                li class=(format!("row relative flex flex-wrap items-center gap-x-3 gap-y-1 px-3 py-2.5 hover:bg-bar/50 {HISTORY_GRID}")) {
                                    @if pick {
                                        label class="pick grid size-7 place-items-center" {
                                            input type="checkbox" name="id" value=(row.job.id.get()) class="size-4 accent-[var(--color-ribbon)]";
                                            span class="sr-only" { "Select " (row.name()) }
                                        }
                                    } @else {
                                        span class="pick-spacer hidden md:block" aria-hidden="true" {}
                                    }
                                    (history_row(row, m.now))
                                }
                            }
                        }
                    }
                    @if any_retryable || m.counts.failed > 0 {
                        div class="bulkbar sticky bottom-3 flex flex-wrap gap-2 self-start rounded-lg border border-rule bg-sheet/95 p-2 shadow-lg backdrop-blur" {
                            @if any_retryable { button class="btn btn-primary" type="submit" { "Retry selected" } }
                            @if m.counts.failed > 0 {
                                button class="btn" type="submit" formaction="/jobs/retry-failed" {
                                    "Retry all " (m.counts.failed) " failed"
                                }
                            }
                        }
                    }
                }
                (pager(m.page, m.has_next, |p| history_href(m.filter, p)))
            }
        }
    }
}

/// Previous and next links.
fn pager(page: u32, has_next: bool, href: impl Fn(u32) -> String) -> Markup {
    html! {
        @if page > 1 || has_next {
            nav class="pager mt-4 flex items-center justify-between gap-3" aria-label="Pages" {
                @if page > 1 { a class="btn" rel="prev" href=(href(page - 1)) { "← Previous" } } @else { span {} }
                span class="text-xs text-muted" { "Page " (page) }
                @if has_next { a class="btn" rel="next" href=(href(page + 1)) { "Next →" } } @else { span {} }
            }
        }
    }
}

#[must_use]
pub fn history_screen<'a>(m: &HistoryModel, url: &'a str) -> Screen<'a> {
    Screen {
        title: "History",
        nav: Nav::History,
        url,
        job: None,
        site: m.site,
        now: m.now,
        above: bar(m.site, m.now, None),
        content: history_content(m),
        partials: Vec::new(),
    }
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

fn fact(label: &str, value: &Markup) -> Markup {
    html! { div class="flex justify-between gap-4 py-1.5" { dt class="text-muted" { (label) } dd class="text-right" { (value) } } }
}

#[must_use]
pub fn job_content(m: &JobModel) -> Markup {
    let job = &m.row.job;
    let status = status_of(job);
    let log_runs = runs(&m.logs);
    html! {
        a class="back text-sm" href="/jobs" { "← All jobs" }
        div class="jobgrid grid grid-cols-1 items-start gap-6 lg:grid-cols-[22rem_minmax(0,1fr)]" {
            div class="jobside flex flex-col gap-4" {
                div class=(format!("head {} {} panel overflow-hidden", status.class, tone(status.class))) {
                    div class="h-1 bg-[var(--tone)]" aria-hidden="true" {}
                    div class="flex flex-col gap-3 p-4" {
                        h1 class="display text-2xl leading-tight font-semibold" { (m.row.name()) }
                        p class="sub flex flex-wrap items-center gap-2 text-sm" {
                            (status_chip(job))
                            @if m.row.title.is_some() {
                                span class="mono text-muted" { (if m.row.is_series() { "" } else { "work " }) (m.row.id_label()) }
                            }
                            a href=(job.target.canonical_url()) rel="noopener noreferrer" { "Open on AO3" }
                        }
                        dl class="facts divide-y divide-rule text-sm" {
                            (fact("Source", &html! { (source_label(job.source)) }))
                            @if let Some(by) = job.resolved_by {
                                (fact("Fixed by", &html! { a href=(format!("/jobs/{by}")) { "job #" (by) } }))
                            }
                            (fact("Failed attempts", &html! { (job.attempt) " of " (m.max_attempts) }))
                            @if job.state == JobState::Queued {
                                (fact("Next try", &when(m.now, job.next_attempt_at)))
                            }
                            @if job.force && job.state.is_active() {
                                (fact("Download", &html! { "from scratch" }))
                            }
                            (fact("Updated", &when(m.now, job.updated_at)))
                            (fact("Added", &when(m.now, job.created_at)))
                            @if let Some(book) = &m.book {
                                (fact("File", &html! { span class="mono break-all" { (book.file_name) } }))
                            }
                        }
                        @if let Some(error) = job.last_error.as_deref().filter(|_| job.state != JobState::Succeeded) {
                            p class="err rounded-md bg-err/8 p-2 font-mono text-xs break-words text-err" { (truncate(first_line(error), 400)) }
                        }
                        @if can_force(job) || job.state == JobState::Queued {
                            div class="actions flex flex-wrap gap-2" {
                                @if is_retryable(job) {
                                    form method="post" action=(format!("/jobs/{}/retry", job.id)) {
                                        button class="btn btn-primary" type="submit" { "Retry" }
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
                                        button class="btn btn-danger" type="submit" { "Cancel" }
                                    }
                                }
                            }
                        }
                    }
                }
                (delete_controls(m))
            }
            section class="min-w-0" {
                (section_head("Log", None, (log_runs.len() > 1).then(|| html! { span class="hint text-xs text-muted" { "newest first" } })))
                @if log_runs.is_empty() {
                    p class="empty" { "No log lines yet." }
                } @else {
                    div class="log sprockets greenbar" tabindex="0" role="log" aria-label="Job log" {
                        // Newest run first; lines within a run stay in order.
                        @for (n, run) in log_runs.iter().enumerate().rev() {
                            div class="run py-1" {
                                h3 class="sticky top-0 bg-sheet/95 px-2 py-1 font-sans text-[0.7rem] font-semibold tracking-wider text-muted uppercase" {
                                    "Run " (n + 1) @if let Some(first) = run.first() { " · " (when(m.now, first.at)) }
                                }
                                @for line in *run {
                                    div class="line" {
                                        time class="text-muted" datetime=(iso(line.at)) title=(iso(line.at)) { (clock(line.at)) }
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
    }
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
    danger_zone(
        &action,
        button,
        explain,
        &html! { input type="hidden" name="job" value=(job.id); },
    )
}

/// A folded-away destructive action.
fn danger_zone(action: &str, button: &str, explain: &str, hidden: &Markup) -> Markup {
    html! {
        details class="danger group rounded-lg border border-rule px-4 py-2 text-sm open:border-err/40" {
            summary class="cursor-pointer text-muted select-none group-open:text-err" { "Delete…" }
            div class="flex flex-col gap-3 py-2" {
                p class="text-muted" { (explain) }
                form method="post" action=(action) {
                    (hidden)
                    button class="btn btn-danger" type="submit" { (button) }
                }
            }
        }
    }
}

#[must_use]
pub fn job_screen<'a>(m: &JobModel, name: &'a str, url: &'a str) -> Screen<'a> {
    Screen {
        title: name,
        nav: Nav::None,
        url,
        job: Some(m.row.job.id),
        site: m.site,
        now: m.now,
        above: bar(m.site, m.now, None),
        content: job_content(m),
        partials: Vec::new(),
    }
}

// ----------------------------------------------------------------------- errors

#[must_use]
pub fn error_page(problem: &ProblemDetails) -> Markup {
    let content = html! {
        div class="mx-auto flex max-w-md flex-col items-start gap-3 py-10" {
            p class="eyebrow" { "Error " (problem.status) }
            h1 class="display text-3xl font-semibold" { (problem.title) }
            @if let Some(detail) = &problem.detail { p class="text-muted" { (detail) } }
            a class="btn" href="/" { "Back to the dashboard" }
        }
    };
    // A plain page: nothing on it changes, so there is nothing to refresh.
    let screen = Screen {
        title: &problem.title,
        nav: Nav::None,
        url: "",
        job: None,
        site: SiteStatus::Healthy,
        now: Timestamp::UNIX_EPOCH,
        above: html! {},
        content,
        partials: Vec::new(),
    };
    full_page(&screen, None)
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
