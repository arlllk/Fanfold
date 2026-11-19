//! The library: every book on a shelf, and one book's page.
//!
//! Covers are drawn in CSS (no images, nothing inline: the CSP forbids it). Each one takes a
//! stable colour from its first fandom (`.hue-N`), so a fandom's books look alike.

use fanfold_app::ports::{
    LibraryEntry, LibraryPage, LibraryQuery, LibrarySort, LibraryStatus, SiteStatus,
};
use fanfold_domain::{
    Timestamp,
    metadata::{TagKind, WorkState},
};
use maud::{Markup, html};

use super::{Frame, Nav, Notice, bar, live, page, when};
use crate::format::{day, grouped, hue, rating_badge, short, summary_paragraphs};

#[derive(Debug)]
pub struct LibraryModel {
    pub now: Timestamp,
    pub site: SiteStatus,
    pub query: LibraryQuery,
    /// 1-based.
    pub page: u32,
    pub result: LibraryPage,
}

impl LibraryModel {
    fn has_next(&self) -> bool {
        u64::from(self.query.offset) + (self.result.entries.len() as u64) < self.result.matching
    }

    fn search(&self) -> &str {
        self.query.search.as_deref().map_or("", str::trim)
    }
}

#[derive(Debug)]
pub struct BookModel {
    pub now: Timestamp,
    pub site: SiteStatus,
    pub entry: LibraryEntry,
    /// Everything recorded about the work (all its tags, its chapters).
    pub state: Option<WorkState>,
}

const STATUS_LABELS: [(LibraryStatus, &str); 3] = [
    (LibraryStatus::All, "All"),
    (LibraryStatus::Complete, "Complete"),
    (LibraryStatus::InProgress, "In progress"),
];

const SORT_LABELS: [(LibrarySort, &str); 4] = [
    (LibrarySort::Updated, "Recently updated"),
    (LibrarySort::Title, "Title"),
    (LibrarySort::Author, "Author"),
    (LibrarySort::Words, "Longest"),
];

/// A link to the library with these settings; defaults are left out of the URL.
fn library_href(search: &str, status: LibraryStatus, sort: LibrarySort, page: u32) -> String {
    let mut parts: Vec<String> = Vec::new();
    if !search.is_empty() {
        parts.push(format!("q={}", form_encode(search)));
    }
    if status != LibraryStatus::All {
        parts.push(format!("status={}", status.as_str()));
    }
    if sort != LibrarySort::Updated {
        parts.push(format!("sort={}", sort.as_str()));
    }
    if page > 1 {
        parts.push(format!("page={page}"));
    }
    if parts.is_empty() {
        "/library".to_owned()
    } else {
        format!("/library?{}", parts.join("&"))
    }
}

/// `application/x-www-form-urlencoded`, as a browser submits the search form.
fn form_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'*' => {
                out.push(char::from(b));
            }
            b' ' => out.push('+'),
            _ => {
                use std::fmt::Write as _;
                let _ = write!(out, "%{b:02X}");
            }
        }
    }
    out
}

/// The colour seed: the first fandom, else the title.
fn cover_hue(entry: &LibraryEntry) -> u32 {
    let seed = entry
        .tags
        .iter()
        .find(|t| t.kind == TagKind::Fandom)
        .map_or_else(|| entry.title(), |t| t.name.as_str());
    hue(seed)
}

fn byline(entry: &LibraryEntry) -> Option<&str> {
    entry
        .facts
        .as_ref()
        .map(|f| f.byline.as_str())
        .filter(|b| !b.is_empty())
}

fn fandoms(entry: &LibraryEntry) -> String {
    entry
        .tags
        .iter()
        .filter(|t| t.kind == TagKind::Fandom)
        .map(|t| t.name.as_str())
        .collect::<Vec<_>>()
        .join(" · ")
}

/// The CSS-drawn cover. Decorative: the same text is beside it.
fn cover(entry: &LibraryEntry, big: bool) -> Markup {
    let class = format!(
        "cover hue-{}{}",
        cover_hue(entry),
        if big { " big" } else { "" }
    );
    html! {
        span class=(class) aria-hidden="true" {
            span class="cover-title" { (entry.title()) }
            @if let Some(by) = byline(entry) { span class="cover-by" { (by) } }
        }
    }
}

fn rating(entry: &LibraryEntry) -> Markup {
    let Some(f) = &entry.facts else {
        return html! {};
    };
    let (class, letter) = rating_badge(&f.rating);
    html! {
        span class=(format!("rating r-{class}")) title=(f.rating) {
            span class="sr-only" { "Rated " (f.rating) }
            span aria-hidden="true" { (letter) }
        }
    }
}

fn progress(entry: &LibraryEntry) -> Markup {
    let Some(f) = &entry.facts else {
        return html! { span class="chip" { "Not checked yet" } };
    };
    let total = f
        .chapters_total
        .map_or_else(|| "?".to_owned(), |t| t.to_string());
    html! {
        @if entry.complete() == Some(true) {
            span class="chip done" { "Complete" }
        } @else {
            span class="chip wip" { "In progress" }
        }
        span { (f.chapters_posted) "/" (total) " ch" }
    }
}

fn shelf_item(entry: &LibraryEntry) -> Markup {
    let fandom_line = fandoms(entry);
    html! {
        li {
            a class="book" href=(format!("/library/{}", entry.book.work)) {
                (cover(entry, false))
                span class="info" {
                    span class="t" { (entry.title()) }
                    @if let Some(by) = byline(entry) { span class="by" { "by " (by) } }
                    @if !fandom_line.is_empty() { span class="fandom" { (fandom_line) } }
                    span class="meta" {
                        (rating(entry))
                        (progress(entry))
                        @if let Some(f) = &entry.facts { span { (short(f.words)) " words" } }
                    }
                    span class="meta soft" {
                        @if let Some(updated) = entry.facts.as_ref().and_then(|f| f.updated_on) {
                            span { "Updated " (day(updated)) }
                        }
                        @if let Some(s) = entry.series.first() {
                            span class="series" {
                                @if let Some(part) = s.part { "Part " (part) " of " }
                                (s.name)
                            }
                        }
                        @if let Some(kudos) = entry.stats.kudos {
                            span class="kudos" { span aria-hidden="true" { "♥ " } (short(kudos)) span class="sr-only" { " kudos" } }
                        }
                        @if entry.facts.as_ref().is_some_and(|f| f.restricted) {
                            span class="lock" title="Visible to logged-in AO3 users only" { "Restricted" }
                        }
                    }
                }
            }
        }
    }
}

/// The library as a live update.
#[must_use]
pub fn library_content(m: &LibraryModel) -> Markup {
    let totals = m.result.totals;
    let search = m.search();
    let filtered = !search.is_empty() || m.query.status != LibraryStatus::All;
    html! {
        p class="lib-stats" {
            span { strong { (grouped(totals.books)) } (if totals.books == 1 { " book" } else { " books" }) }
            span { strong { (short(totals.words)) } " words" }
            span { strong { (grouped(totals.complete)) } " complete" }
        }
        nav class="seg" aria-label="Completion" {
            @for (status, label) in STATUS_LABELS {
                a href=(library_href(search, status, m.query.sort, 1))
                  aria-current=[(status == m.query.status).then_some("page")] { (label) }
            }
        }
        @if filtered {
            p class="lib-count" role="status" {
                (grouped(m.result.matching)) " of " (grouped(totals.books))
                @if search.is_empty() { " books" } @else { " books match “" (search) "”" }
            }
        }
        @if m.result.entries.is_empty() {
            @if totals.books == 0 {
                p class="empty" { "Your library is empty. Paste an AO3 link on the " a href="/" { "dashboard" } " and the book appears here once it is downloaded." }
            } @else {
                p class="empty" { "No books match. " a href="/library" { "Show everything" } }
            }
        } @else {
            ul class="books" { @for entry in &m.result.entries { (shelf_item(entry)) } }
        }
        @if m.page > 1 || m.has_next() {
            nav class="pager" aria-label="Pages" {
                @if m.page > 1 {
                    a class="btn" rel="prev" href=(library_href(search, m.query.status, m.query.sort, m.page - 1)) { "← Newer" }
                }
                span { "Page " (m.page) }
                @if m.has_next() {
                    a class="btn" rel="next" href=(library_href(search, m.query.status, m.query.sort, m.page + 1)) { "Older →" }
                }
            }
        }
    }
}

/// The heading, search and order: outside the live region, so a refresh never eats what you
/// type.
fn library_tools(m: &LibraryModel) -> Markup {
    html! {
        h1 class="lib-title" { "Library" }
        form class="lib-tools" method="get" action="/library" role="search" {
            @if m.query.status != LibraryStatus::All {
                input type="hidden" name="status" value=(m.query.status.as_str());
            }
            label class="sr-only" for="q" { "Search the library" }
            input id="q" name="q" type="search" value=(m.search()) autocomplete="off"
                  placeholder="Title, author, tag or series";
            label class="sr-only" for="sort" { "Order" }
            select id="sort" name="sort" {
                @for (sort, label) in SORT_LABELS {
                    option value=(sort.as_str()) selected[sort == m.query.sort] { (label) }
                }
            }
            button class="btn primary" type="submit" { "Search" }
        }
    }
}

#[must_use]
pub fn library(m: &LibraryModel) -> Markup {
    live(m.site, m.now, &library_content(m))
}

#[must_use]
pub fn library_page(m: &LibraryModel, notice: Option<Notice>) -> Markup {
    page(
        Frame {
            title: "Library",
            nav: Nav::Library,
            job: None,
            notice,
        },
        &html! { (bar(m.site, m.now, None)) (library_tools(m)) },
        &library_content(m),
    )
}

// ------------------------------------------------------------------- one book

const TAG_SECTIONS: [(TagKind, &str); 5] = [
    (TagKind::Warning, "Warnings"),
    (TagKind::Category, "Categories"),
    (TagKind::Relationship, "Relationships"),
    (TagKind::Character, "Characters"),
    (TagKind::Freeform, "Tags"),
];

fn stat(label: &str, value: Option<String>) -> Markup {
    html! { @if let Some(value) = value { div { dt { (label) } dd { (value) } } } }
}

/// Cover, title, byline, badges and the two actions.
fn work_head(entry: &LibraryEntry) -> Markup {
    let work = entry.book.work;
    let fandom_line = fandoms(entry);
    html! {
        header class="work-head" {
            (cover(entry, true))
            div class="work-id" {
                h1 { (entry.title()) }
                @if let Some(by) = byline(entry) { p class="by" { "by " (by) } }
                @if !fandom_line.is_empty() { p class="fandom" { (fandom_line) } }
                p class="meta" {
                    (rating(entry))
                    (progress(entry))
                    @if entry.facts.as_ref().is_some_and(|f| f.restricted) {
                        span class="lock" title="Visible to logged-in AO3 users only" { "Restricted" }
                    }
                }
                div class="actions" {
                    a class="btn" href=(work.canonical_url()) target="_blank" rel="noreferrer noopener" { "Open on AO3" }
                    form method="post" action="/jobs" {
                        input type="hidden" name="url" value=(work.canonical_url());
                        button class="btn primary" type="submit" { "Check for updates" }
                    }
                }
            }
        }
    }
}

/// Every current tag, by kind, and the chapter list.
fn work_tags(state: &WorkState) -> Markup {
    html! {
        @for (kind, heading) in TAG_SECTIONS {
            @let names: Vec<&str> = state.tags.iter().filter(|t| t.kind == kind).map(|t| t.name.as_str()).collect();
            @if !names.is_empty() {
                section class="tags" {
                    h2 { (heading) }
                    ul { @for name in names { li { (name) } } }
                }
            }
        }
        @if !state.chapters.is_empty() {
            details class="chapters" {
                summary { "Chapters (" (state.chapters.len()) ")" }
                ol {
                    @for ch in &state.chapters {
                        li {
                            span { (ch.title) }
                            @if let Some(posted) = ch.posted_on { span class="soft" { (day(posted)) } }
                        }
                    }
                }
            }
        }
    }
}

#[must_use]
pub fn book_content(m: &BookModel) -> Markup {
    let entry = &m.entry;
    let work = entry.book.work;
    let facts = entry.facts.as_ref();
    html! {
        article class="work" {
            (work_head(entry))

            @if let Some(f) = facts {
                @let paragraphs = summary_paragraphs(&f.summary_html);
                @if !paragraphs.is_empty() {
                    section class="summary" {
                        h2 class="sr-only" { "Summary" }
                        @for p in &paragraphs { p { (p) } }
                    }
                }
            } @else {
                p class="empty" { "This work has not been checked on AO3 yet, so only its file is known. “Check for updates” fetches the rest." }
            }

            dl class="stats" {
                (stat("Words", facts.map(|f| grouped(f.words))))
                (stat("Chapters", facts.map(|f| format!("{}/{}", f.chapters_posted, f.chapters_total.map_or_else(|| "?".to_owned(), |t| t.to_string())))))
                (stat("Kudos", entry.stats.kudos.map(grouped)))
                (stat("Hits", entry.stats.hits.map(grouped)))
                (stat("Comments", entry.stats.comments.map(grouped)))
                (stat("Bookmarks", entry.stats.bookmarks.map(grouped)))
                (stat("Published", facts.and_then(|f| f.published_on).map(day)))
                (stat("Updated", facts.and_then(|f| f.updated_on).map(day)))
                (stat("Language", facts.map(|f| f.language.clone()).filter(|l| !l.is_empty())))
                (stat("Rating", facts.map(|f| f.rating.clone()).filter(|r| !r.is_empty())))
            }

            @if !entry.series.is_empty() {
                section class="work-series" {
                    h2 { "Series" }
                    ul {
                        @for s in &entry.series {
                            li {
                                @if let Some(part) = s.part { "Part " (part) " of " }
                                a href=(s.id.canonical_url()) target="_blank" rel="noreferrer noopener" { (s.name) }
                            }
                        }
                    }
                }
            }

            @if let Some(state) = &m.state { (work_tags(state)) }

            section class="file" {
                h2 { "In your library" }
                p class="mono" { (entry.book.file_name) }
                p class="soft" {
                    "Saved " (when(m.now, entry.book.updated_at))
                    @if let Some(checked) = entry.last_checked { " · checked on AO3 " (when(m.now, checked)) }
                }
            }

            details class="danger" {
                summary { "Delete…" }
                p { "Removes the EPUB from your library and cancels queued downloads of it. Its history is kept; a new download (or an AO3 email) brings it back." }
                form method="post" action=(format!("/works/{work}/delete")) {
                    input type="hidden" name="back" value="library";
                    button class="btn danger" type="submit" { "Delete the book" }
                }
            }
        }
    }
}

#[must_use]
pub fn book(m: &BookModel) -> Markup {
    live(m.site, m.now, &book_content(m))
}

#[must_use]
pub fn book_page(m: &BookModel, notice: Option<Notice>) -> Markup {
    page(
        Frame {
            title: m.entry.title(),
            nav: Nav::Library,
            job: None,
            notice,
        },
        &html! { (bar(m.site, m.now, None)) a class="back" href="/library" { "← Library" } },
        &book_content(m),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn library_links_leave_defaults_out() {
        assert_eq!(
            library_href("", LibraryStatus::All, LibrarySort::Updated, 1),
            "/library"
        );
        assert_eq!(
            library_href("ada & ben", LibraryStatus::Complete, LibrarySort::Words, 2),
            "/library?q=ada+%26+ben&status=complete&sort=words&page=2"
        );
        assert_eq!(form_encode("é/?"), "%C3%A9%2F%3F");
    }
}
