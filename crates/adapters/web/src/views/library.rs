//! The library: the books a search finds (by book, series, author or fandom), one book's
//! page and one series' page.
//!
//! Everything a search does lives in its text (`fanfold_app::search`): a chip, a filter or a
//! suggestion is a link to the same tab with another query, and, with scripts on, writes that
//! query into the search box instead (`data-q`), which searches as you type. Covers are drawn
//! in CSS; each takes a stable colour from its first fandom (`.hue-N`).

use fanfold_app::{
    browse::{BookList, Facets, Group, GroupKind, LibrarySort, LibraryTotals, SeriesView},
    ports::{LibraryEntry, SiteStatus, Suggestion, SuggestionKind},
    search::{Clause, Flag, Rating, SearchQuery, TextField, Token, WordRange},
};
use fanfold_domain::{
    Timestamp,
    metadata::{TagKind, WorkState},
    work::SeriesId,
};
use maud::{Markup, html};

use super::{Nav, Screen, bar, danger_zone, pager, when};
use crate::format::{day, grouped, hue, rating_badge, short, summary_paragraphs};

// ------------------------------------------------------------------------------ the tabs

/// The library's tabs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tab {
    Books,
    Series,
    Authors,
    Fandoms,
}

impl Tab {
    pub const ALL: [Self; 4] = [Self::Books, Self::Series, Self::Authors, Self::Fandoms];

    #[must_use]
    pub const fn path(self) -> &'static str {
        match self {
            Self::Books => "/library",
            Self::Series => "/library/series",
            Self::Authors => "/library/authors",
            Self::Fandoms => "/library/fandoms",
        }
    }

    const fn label(self) -> &'static str {
        match self {
            Self::Books => "Books",
            Self::Series => "Series",
            Self::Authors => "Authors",
            Self::Fandoms => "Fandoms",
        }
    }
}

/// What every tab shows around its results.
#[derive(Debug)]
pub struct LibraryFrame {
    pub now: Timestamp,
    pub site: SiteStatus,
    pub tab: Tab,
    pub query: SearchQuery,
    pub totals: LibraryTotals,
    /// What the word being typed could be.
    pub suggestions: Vec<Suggestion>,
}

#[derive(Debug)]
pub struct BooksModel {
    pub frame: LibraryFrame,
    /// The order asked for in the URL (`None`: the natural one, see `LibrarySort::effective`).
    pub sort: Option<LibrarySort>,
    /// 1-based.
    pub page: u32,
    /// Books before this page.
    pub offset: u32,
    pub result: BookList,
}

impl BooksModel {
    fn has_next(&self) -> bool {
        u64::from(self.offset) + (self.result.entries.len() as u64) < self.result.matching
    }
}

#[derive(Debug)]
pub struct SeriesListModel {
    pub frame: LibraryFrame,
    pub series: Vec<SeriesView>,
}

#[derive(Debug)]
pub struct GroupsModel {
    pub frame: LibraryFrame,
    pub kind: GroupKind,
    pub groups: Vec<Group>,
}

// ---------------------------------------------------------------------------- links

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

/// The query as the search box shows it.
fn query_text(query: &SearchQuery) -> String {
    query.render().trim_end().to_owned()
}

/// A link to a tab with this query; defaults are left out of the URL.
#[must_use]
pub fn library_href(tab: Tab, query: &SearchQuery, sort: Option<LibrarySort>, page: u32) -> String {
    let mut parts: Vec<String> = Vec::new();
    let search = query_text(query);
    if !search.is_empty() {
        parts.push(format!("q={}", form_encode(&search)));
    }
    if let Some(sort) = sort {
        parts.push(format!("sort={}", sort.as_str()));
    }
    if page > 1 {
        parts.push(format!("page={page}"));
    }
    if parts.is_empty() {
        tab.path().to_owned()
    } else {
        format!("{}?{}", tab.path(), parts.join("&"))
    }
}

/// A link that changes the search: a plain link without scripts; with them, the search box
/// takes the new query (and searches) instead of the page reloading.
fn query_link(tab: Tab, query: &SearchQuery, class: &str, current: bool, body: &Markup) -> Markup {
    let text = query_text(query);
    html! {
        a class=(class) href=(library_href(tab, query, None, 1)) data-q=(text)
          aria-current=[current.then_some("true")] { (body) }
    }
}

/// The search's filter on a book's tag, author or series: picks it exactly.
fn exact(field: TextField, value: &str) -> SearchQuery {
    SearchQuery {
        tokens: vec![Token::new(Clause::Exact {
            field,
            value: value.to_lowercase(),
        })],
        typing: false,
    }
}

fn exact_href(field: TextField, value: &str) -> String {
    library_href(Tab::Books, &exact(field, value), None, 1)
}

// --------------------------------------------------------------------------- the frame

/// The orders one can ask for; the natural one (best match while searching, else most
/// recently updated) is the menu's first, empty choice.
const SORT_LABELS: [(LibrarySort, &str); 5] = [
    (LibrarySort::Updated, "Recently updated"),
    (LibrarySort::Title, "Title"),
    (LibrarySort::Author, "Author"),
    (LibrarySort::Words, "Longest"),
    (LibrarySort::Kudos, "Most kudos"),
];

/// Above the live region: the heading and the search box, so a refresh never touches what
/// is being typed.
fn library_above(f: &LibraryFrame, sort: Option<LibrarySort>) -> Markup {
    let path = f.tab.path();
    html! {
        (bar(f.site, f.now, None))
        div class="flex flex-wrap items-baseline justify-between gap-x-6 gap-y-1" {
            h1 class="lib-title display text-4xl font-semibold" { "Library" }
            p class="lib-stats flex gap-4 text-sm text-muted [&_strong]:text-ink [&_strong]:tabular-nums" {
                span { strong { (grouped(f.totals.books)) } (if f.totals.books == 1 { " book" } else { " books" }) }
                span { strong { (short(f.totals.words)) } " words" }
                span { strong { (grouped(f.totals.complete)) } " complete" }
            }
        }
        form id="lib-search" class="lib-tools flex flex-wrap gap-2" method="get" action=(path) role="search"
             hx-get=(path) hx-target="#live" hx-swap="outerMorph" hx-push-url="true" hx-sync="#live:replace"
             x-data="search" {
            div class="relative min-w-0 basis-full sm:basis-0 sm:flex-1" {
                label class="sr-only" for="q" { "Search the library" }
                input id="q" name="q" type="search" value=(query_text(&f.query)) autocomplete="off"
                      spellcheck="false" autocapitalize="off" class="field h-11 pr-10 text-base"
                      placeholder="Search titles, authors, tags… or author:name, rating:e, words:>100k"
                      role="combobox" aria-autocomplete="list" aria-controls="suggest" aria-expanded="false"
                      x-bind:aria-expanded="expandedText" x-bind:aria-activedescendant="activeId"
                      hx-get=(path) hx-include="closest form" hx-target="#live" hx-swap="outerMorph"
                      hx-push-url="true" hx-trigger="input changed delay:150ms" hx-sync="#live:replace"
                      "x-on:keydown"="key" "x-on:focus"="show" "x-on:blur"="hide";
                span class="htmx-indicator pointer-events-none absolute top-1/2 right-3 size-4 -translate-y-1/2 animate-spin rounded-full border-2 border-rule border-t-ribbon" aria-hidden="true" {}
                ul id="suggest" class="listbox" role="listbox" aria-label="Suggestions"
                   x-show="expanded" x-cloak "x-on:mousedown"="hold" "x-on:click"="pick" {
                    (suggestion_items(f))
                }
            }
            @if f.tab == Tab::Books {
                label class="sr-only" for="sort" { "Order" }
                select id="sort" name="sort" class="field h-11 w-auto flex-1 text-sm sm:flex-none"
                       hx-get=(path) hx-include="closest form" hx-target="#live" hx-swap="outerMorph"
                       hx-push-url="true" hx-sync="#live:replace" {
                    option value="" selected[sort.is_none()] {
                        (if f.query.has_text() { "Best match" } else { "Recently updated" })
                    }
                    @for (s, label) in SORT_LABELS.iter().filter(|(s, _)| f.query.has_text() || *s != LibrarySort::Updated) {
                        option value=(s.as_str()) selected[Some(*s) == sort] { (label) }
                    }
                }
            }
            button class="btn h-11" type="submit" { "Search" }
        }
    }
}

const fn kind_label(kind: SuggestionKind) -> &'static str {
    match kind {
        SuggestionKind::Author => "Author",
        SuggestionKind::Series => "Series",
        SuggestionKind::Fandom => "Fandom",
        SuggestionKind::Relationship => "Ship",
        SuggestionKind::Character => "Character",
        SuggestionKind::Tag => "Tag",
        SuggestionKind::Warning => "Warning",
        SuggestionKind::Language => "Language",
        SuggestionKind::Book => "Book",
    }
}

/// The suggestions for the word being typed: picking one writes it into the search exactly
/// (or, for a book, opens it).
#[must_use]
pub fn suggestion_items(f: &LibraryFrame) -> Markup {
    html! {
        @for (i, s) in f.suggestions.iter().enumerate() {
            @let id = format!("sug-{i}");
            @let body = html! {
                span class="kind" { (kind_label(s.kind)) }
                span class="min-w-0 flex-1 truncate" { (s.label) }
                @if s.kind != SuggestionKind::Book { span class="text-xs text-muted tabular-nums" { (s.books) } }
            };
            @if let Some(work) = s.work {
                a id=(id) role="option" aria-selected="false" href=(format!("/library/{work}")) { (body) }
            } @else if let Some(field) = s.kind.field() {
                @let next = f.query.completed_with(Token::new(Clause::Exact { field, value: s.value.clone() }));
                li id=(id) role="option" aria-selected="false" data-q=(query_text(&next)) { (body) }
            }
        }
    }
}

fn tabs(f: &LibraryFrame, count: u64) -> Markup {
    html! {
        nav class="tabs -mb-px flex gap-1 overflow-x-auto border-b border-rule" aria-label="Library views" {
            @for tab in Tab::ALL {
                @let here = tab == f.tab;
                a href=(library_href(tab, &f.query, None, 1)) aria-current=[here.then_some("page")]
                  class="border-b-2 border-transparent px-3 py-2 text-sm font-medium whitespace-nowrap text-muted no-underline hover:text-ink hover:no-underline aria-[current=page]:border-ribbon aria-[current=page]:text-ink" {
                    (tab.label())
                    @if here { " " span class="mono text-muted tabular-nums" { (grouped(count)) } }
                }
            }
        }
    }
}

// --------------------------------------------------------------------------- the chips

const fn field_name(field: TextField) -> &'static str {
    match field {
        TextField::Any => "",
        TextField::Title => "Title",
        TextField::Author => "Author",
        TextField::Series => "Series",
        TextField::Fandom => "Fandom",
        TextField::Relationship => "Ship",
        TextField::Character => "Character",
        TextField::Tag => "Tag",
        TextField::Warning => "Warning",
        TextField::Language => "Language",
        TextField::Summary => "Summary",
    }
}

const fn flag_label(flag: Flag) -> &'static str {
    match flag {
        Flag::Complete => "Complete",
        Flag::InProgress => "In progress",
        Flag::Restricted => "Restricted",
    }
}

const fn rating_label(r: Rating) -> &'static str {
    match r {
        Rating::General => "General",
        Rating::Teen => "Teen",
        Rating::Mature => "Mature",
        Rating::Explicit => "Explicit",
        Rating::NotRated => "Not rated",
    }
}

fn range_label(r: WordRange) -> String {
    match (r.min, r.max) {
        (Some(lo), None) => format!("{}+ words", short(lo)),
        (None, Some(hi)) => format!("up to {} words", short(hi)),
        (Some(lo), Some(hi)) => format!("{}–{} words", short(lo), short(hi)),
        (None, None) => "any length".to_owned(),
    }
}

/// What a token means, in words.
fn token_label(t: &Token) -> Markup {
    let field_prefix = |field: TextField| {
        html! { @if field != TextField::Any { span class="text-muted" { (field_name(field)) ": " } } }
    };
    html! {
        @if t.negated { span class="font-semibold text-err" { "not " } }
        @match &t.clause {
            Clause::Words { field, text } => { (field_prefix(*field)) (text) }
            Clause::Phrase { field, text } => { (field_prefix(*field)) "“" (text) "”" }
            Clause::Exact { field, value } => { (field_prefix(*field)) strong class="font-medium" { (value) } }
            Clause::Rating(ratings) => {
                span class="text-muted" { "Rating: " }
                (ratings.iter().map(|r| rating_label(*r)).collect::<Vec<_>>().join(", "))
            }
            Clause::WordCount(range) => { (range_label(*range)) }
            Clause::Is(flag) => { (flag_label(*flag)) }
        }
    }
}

/// The search, token by token, each one removable.
fn chips(f: &LibraryFrame) -> Markup {
    html! {
        @if !f.query.is_empty() {
            div class="chips flex flex-wrap items-center gap-1.5" aria-label="Search filters" {
                @for token in &f.query.tokens {
                    @let rest = f.query.without(|t| t == token);
                    (query_link(f.tab, &rest, if token.negated { "filter-chip not" } else { "filter-chip" }, false, &html! {
                        span { (token_label(token)) }
                        span class="x" aria-hidden="true" { "×" }
                        span class="sr-only" { " (remove)" }
                    }))
                }
                (query_link(f.tab, &SearchQuery::default(), "ml-1 text-xs", false, &html! { "Clear all" }))
            }
        }
    }
}

// ------------------------------------------------------------------------- the filters

/// The completion filter of the query (`None`: either).
fn status_of_query(query: &SearchQuery) -> Option<Flag> {
    query.tokens.iter().find_map(|t| match t.clause {
        Clause::Is(flag @ (Flag::Complete | Flag::InProgress)) if !t.negated => Some(flag),
        _ => None,
    })
}

fn with_status(query: &SearchQuery, status: Option<Flag>) -> SearchQuery {
    let completion = |t: &Token| matches!(t.clause, Clause::Is(Flag::Complete | Flag::InProgress));
    match status {
        Some(flag) => query.with(Token::new(Clause::Is(flag)), completion),
        None => query.without(completion),
    }
}

fn ratings_of(query: &SearchQuery) -> Vec<Rating> {
    query
        .tokens
        .iter()
        .find_map(|t| match &t.clause {
            Clause::Rating(r) if !t.negated => Some(r.clone()),
            _ => None,
        })
        .unwrap_or_default()
}

fn with_rating_toggled(query: &SearchQuery, rating: Rating) -> SearchQuery {
    let mut ratings = ratings_of(query);
    if let Some(i) = ratings.iter().position(|r| *r == rating) {
        ratings.remove(i);
    } else {
        ratings.push(rating);
        ratings.sort();
    }
    let is_rating = |t: &Token| matches!(t.clause, Clause::Rating(_)) && !t.negated;
    if ratings.is_empty() {
        query.without(is_rating)
    } else {
        query.with(Token::new(Clause::Rating(ratings)), is_rating)
    }
}

const LENGTHS: [(&str, WordRange); 4] = [
    (
        "Under 10k",
        WordRange {
            min: None,
            max: Some(9_999),
        },
    ),
    (
        "10k–50k",
        WordRange {
            min: Some(10_000),
            max: Some(50_000),
        },
    ),
    (
        "50k–150k",
        WordRange {
            min: Some(50_000),
            max: Some(150_000),
        },
    ),
    (
        "Over 150k",
        WordRange {
            min: Some(150_001),
            max: None,
        },
    ),
];

fn exact_token(field: TextField, value: &str, negated: bool) -> Token {
    Token {
        clause: Clause::Exact {
            field,
            value: value.to_lowercase(),
        },
        negated,
    }
}

/// A row of the filter panel: a toggle with a count.
fn option_row(
    f: &LibraryFrame,
    next: &SearchQuery,
    on: bool,
    label: &str,
    count: Option<u32>,
) -> Markup {
    query_link(
        f.tab,
        next,
        "flex items-center gap-2 rounded px-1.5 py-1 text-sm text-ink no-underline hover:bg-bar hover:no-underline aria-[current=true]:font-medium",
        on,
        &html! {
            span class=(format!("grid size-3.5 shrink-0 place-items-center rounded-sm border {}", if on { "border-ribbon bg-ribbon text-sheet" } else { "border-rule" }))
                 aria-hidden="true" { @if on { "✓" } }
            span class="min-w-0 flex-1 truncate" { (label) }
            @if let Some(n) = count { span class="text-xs text-muted tabular-nums" { (n) } }
        },
    )
}

fn filter_group(title: &str, body: &Markup) -> Markup {
    html! {
        section class="flex flex-col gap-1" {
            h2 class="eyebrow mb-1" { (title) }
            (body)
        }
    }
}

fn filters(f: &LibraryFrame, facets: &Facets) -> Markup {
    let q = &f.query;
    let status = status_of_query(q);
    let ratings = ratings_of(q);
    let length = q.tokens.iter().find_map(|t| match t.clause {
        Clause::WordCount(r) if !t.negated => Some(r),
        _ => None,
    });
    let is_length = |t: &Token| matches!(t.clause, Clause::WordCount(_));
    let has = |token: &Token| q.tokens.contains(token);
    let restricted = Token::new(Clause::Is(Flag::Restricted));
    html! {
        div class="flex flex-col gap-5" x-data="disclosure" {
            button type="button" class="btn justify-between lg:hidden" "x-on:click"="toggle" x-bind:aria-expanded="openText" aria-controls="filters" {
                "Filters" span class="text-muted" aria-hidden="true" { "▾" }
            }
            div id="filters" class="flex flex-col gap-5" x-bind:class="panelClass" {
                (filter_group("Completion", &html! {
                    @for (flag, label, n) in [(Some(Flag::Complete), "Complete", facets.complete), (Some(Flag::InProgress), "In progress", facets.in_progress)] {
                        @let on = flag == status;
                        (option_row(f, &with_status(q, if on { None } else { flag }), on, label, Some(n)))
                    }
                }))
                (filter_group("Rating", &html! {
                    @for (rating, n) in &facets.ratings {
                        (option_row(f, &with_rating_toggled(q, *rating), ratings.contains(rating), rating_label(*rating), Some(*n)))
                    }
                }))
                (filter_group("Length", &html! {
                    @for (label, range) in LENGTHS {
                        @let on = length == Some(range);
                        @let next = if on { q.without(is_length) } else { q.with(Token::new(Clause::WordCount(range)), is_length) };
                        (option_row(f, &next, on, label, None))
                    }
                }))
                @if !facets.languages.is_empty() {
                    (filter_group("Language", &html! {
                        @for (language, n) in facets.languages.iter().take(8) {
                            @let token = exact_token(TextField::Language, language, false);
                            @let on = has(&token);
                            @let is_lang = |t: &Token| matches!(&t.clause, Clause::Exact { field: TextField::Language, .. });
                            @let next = if on { q.without(is_lang) } else { q.with(token, is_lang) };
                            (option_row(f, &next, on, language, Some(*n)))
                        }
                    }))
                }
                @if !facets.warnings.is_empty() {
                    (filter_group("Warnings", &html! {
                        @for (warning, n) in &facets.warnings {
                            @let include = exact_token(TextField::Warning, warning, false);
                            @let exclude = exact_token(TextField::Warning, warning, true);
                            @let same = |t: &Token| matches!(&t.clause, Clause::Exact { field: TextField::Warning, value } if *value == warning.to_lowercase());
                            div class="flex items-center gap-1 text-sm" {
                                span class="min-w-0 flex-1 truncate px-1.5" title=(warning) { (warning) }
                                span class="text-xs text-muted tabular-nums" { (n) }
                                @let only = has(&include);
                                @let hide = has(&exclude);
                                (query_link(f.tab, &if only { q.without(same) } else { q.with(include, same) },
                                    "rounded border border-rule px-1.5 py-px text-xs text-muted no-underline hover:border-ribbon hover:text-ink hover:no-underline aria-[current=true]:border-ribbon aria-[current=true]:bg-ribbon-soft aria-[current=true]:text-ink",
                                    only, &html! { "only" span class="sr-only" { " " (warning) } }))
                                (query_link(f.tab, &if hide { q.without(same) } else { q.with(exclude, same) },
                                    "rounded border border-rule px-1.5 py-px text-xs text-muted no-underline hover:border-err hover:text-err hover:no-underline aria-[current=true]:border-err aria-[current=true]:bg-err/10 aria-[current=true]:text-err",
                                    hide, &html! { "hide" span class="sr-only" { " " (warning) } }))
                            }
                        }
                    }))
                }
                @if facets.restricted > 0 || has(&restricted) {
                    (filter_group("Access", &html! {
                        (option_row(f, &q.toggled(&restricted), has(&restricted), "Logged-in users only", Some(facets.restricted)))
                    }))
                }
                details class="text-xs text-muted" {
                    summary class="cursor-pointer select-none" { "Typing filters" }
                    dl class="mt-2 grid grid-cols-[auto_1fr] gap-x-3 gap-y-1 font-mono" {
                        dt { "author:quill" } dd class="font-sans" { "an author's name starts so" }
                        dt { "tag:\"slow burn\"" } dd class="font-sans" { "exactly that tag" }
                        dt { "rating:e,m" } dd class="font-sans" { "Explicit or Mature" }
                        dt { "words:>100k" } dd class="font-sans" { "longer than 100,000 words" }
                        dt { "is:wip" } dd class="font-sans" { "still in progress" }
                        dt { "-warn:mcd" } dd class="font-sans" { "leave those out" }
                    }
                }
            }
        }
    }
}

// --------------------------------------------------------------------------- the books

/// The colour seed: the first fandom, else the title.
fn cover_hue(entry: &LibraryEntry) -> u32 {
    hue(entry
        .tags_of(TagKind::Fandom)
        .next()
        .unwrap_or_else(|| entry.title()))
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
        .tags_of(TagKind::Fandom)
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
            span class="label" { (entry.title()) }
            @if let Some(by) = byline(entry) { span class="by" { (by) } }
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
        return html! { span class="chip tone-muted" { "Not checked yet" } };
    };
    let total = f
        .chapters_total
        .map_or_else(|| "?".to_owned(), |t| t.to_string());
    html! {
        @if entry.complete() == Some(true) {
            span class="chip tone-ok" { "Complete" }
        } @else {
            span class="chip tone-warn" { "In progress" }
        }
        span class="tabular-nums" { (f.chapters_posted) "/" (total) " ch" }
    }
}

fn shelf_item(entry: &LibraryEntry) -> Markup {
    let fandom_line = fandoms(entry);
    html! {
        li {
            a class="book group flex flex-col gap-2 text-ink no-underline hover:no-underline" href=(format!("/library/{}", entry.book.work)) {
                (cover(entry, false))
                span class="info flex min-w-0 flex-col gap-0.5" {
                    span class="t display line-clamp-2 leading-snug font-semibold group-hover:text-ribbon" { (entry.title()) }
                    @if let Some(by) = byline(entry) { span class="by truncate text-sm text-muted" { "by " (by) } }
                    @if !fandom_line.is_empty() { span class="fandom truncate text-xs text-muted" { (fandom_line) } }
                    span class="meta mt-1 flex flex-wrap items-center gap-1.5 text-xs text-muted" {
                        (rating(entry))
                        (progress(entry))
                        @if let Some(f) = &entry.facts { span class="tabular-nums" { (short(f.words)) " words" } }
                    }
                    span class="meta soft flex flex-wrap gap-x-2 text-xs text-muted" {
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

const BOOK_GRID: &str = "books grid grid-cols-[repeat(auto-fill,minmax(8.5rem,1fr))] gap-x-4 gap-y-7 sm:grid-cols-[repeat(auto-fill,minmax(9.5rem,1fr))]";

/// When the search is one exact author or fandom: who or what the list is of.
fn subject(f: &LibraryFrame) -> Markup {
    let [token] = f.query.tokens.as_slice() else {
        return html! {};
    };
    if let (
        Clause::Exact {
            field: TextField::Author,
            value,
        },
        false,
    ) = (&token.clause, token.negated)
    {
        html! {
            p class="flex flex-wrap items-baseline gap-x-3 text-sm text-muted" {
                span { "Books by " strong class="display text-lg text-ink" { (value) } }
                a href=(format!("https://archiveofourown.org/users/{}", form_encode(value))) target="_blank" rel="noreferrer noopener" { "Their AO3 page" }
            }
        }
    } else {
        html! {}
    }
}

fn empty_result(f: &LibraryFrame) -> Markup {
    html! {
        @if f.totals.books == 0 {
            p class="empty" { "Your library is empty. Paste an AO3 link on the " a href="/" { "dashboard" } " and the book appears here once it is downloaded." }
        } @else {
            p class="empty" { "No books match. " (query_link(f.tab, &SearchQuery::default(), "", false, &html! { "Show everything" })) }
        }
    }
}

/// The Books tab as a live update.
#[must_use]
pub fn books_content(m: &BooksModel) -> Markup {
    let f = &m.frame;
    html! {
        (tabs(f, m.result.matching))
        (chips(f))
        @if !f.query.is_empty() {
            p class="lib-count sr-only" role="status" {
                (grouped(m.result.matching)) " of " (grouped(f.totals.books)) " books match “" (query_text(&f.query)) "”"
            }
        }
        div class="grid grid-cols-1 items-start gap-8 lg:grid-cols-[13.5rem_minmax(0,1fr)]" {
            aside class="lg:sticky lg:top-20" aria-label="Filters" { (filters(f, &m.result.facets)) }
            div class="flex min-w-0 flex-col gap-4" {
                (subject(f))
                @if m.result.entries.is_empty() {
                    (empty_result(f))
                } @else {
                    ul class=(BOOK_GRID) { @for entry in &m.result.entries { (shelf_item(entry)) } }
                }
                (pager(m.page, m.has_next(), |p| library_href(Tab::Books, &f.query, m.sort, p)))
            }
        }
    }
}

#[must_use]
pub fn books_screen<'a>(m: &BooksModel, url: &'a str) -> Screen<'a> {
    library_screen(&m.frame, url, books_content(m), m.sort)
}

fn library_screen<'a>(
    f: &LibraryFrame,
    url: &'a str,
    content: Markup,
    sort: Option<LibrarySort>,
) -> Screen<'a> {
    Screen {
        title: f.tab.label(),
        nav: Nav::Library,
        url,
        job: None,
        site: f.site,
        now: f.now,
        above: library_above(f, sort),
        content,
        partials: vec![("#suggest", suggestion_items(f))],
    }
}

// -------------------------------------------------------------------------- the series

/// A small cover for a part of a series; a dashed outline when it is not in the library.
fn part_cover(p: &fanfold_app::browse::SeriesPartView) -> Markup {
    let label = p.part.map(|n| format!("Part {n}"));
    html! {
        li class="flex w-24 shrink-0 flex-col gap-1.5 sm:w-28" {
            @if let Some(entry) = &p.entry {
                a href=(format!("/library/{}", p.work)) class=(format!("block no-underline {}", if p.matches { "" } else { "opacity-45" }))
                  title=(entry.title()) {
                    (cover(entry, false))
                    span class="sr-only" { (entry.title()) }
                }
            } @else {
                span class="cover absent" {
                    span class="label text-[0.7rem] font-medium" { (p.title.as_deref().unwrap_or("Not in your library")) }
                }
            }
            span class="text-[0.7rem] text-muted" { (label.unwrap_or_default()) }
        }
    }
}

fn series_stats(s: &SeriesView) -> Markup {
    let total = s.parts.len();
    html! {
        @if s.listed {
            span class="tabular-nums" { (s.in_library) " of " (total) (if total == 1 { " part" } else { " parts" }) " in your library" }
        } @else {
            span class="tabular-nums" { (s.in_library) (if s.in_library == 1 { " part" } else { " parts" }) " in your library" }
        }
        @match s.complete {
            Some(true) => { span class="chip tone-ok" { "Complete" } }
            Some(false) => { span class="chip tone-warn" { "Ongoing" } }
            None => {}
        }
        @if s.followed { span class="chip tone-info" { "Followed" } }
        @if s.words > 0 { span class="tabular-nums" { (short(s.words)) " words" } }
        @if let Some(updated) = s.updated { span { "Updated " (day(updated)) } }
    }
}

#[must_use]
pub fn series_list_content(m: &SeriesListModel) -> Markup {
    let f = &m.frame;
    html! {
        (tabs(f, m.series.len() as u64))
        (chips(f))
        @if m.series.is_empty() {
            @if f.query.is_empty() {
                p class="empty" { "No series yet. A book that belongs to one brings it here, or add a series link on the " a href="/" { "dashboard" } "." }
            } @else {
                p class="empty" { "No series has a book that matches. " (query_link(f.tab, &SearchQuery::default(), "", false, &html! { "Show every series" })) }
            }
        } @else {
            ul class="series-list flex flex-col gap-5" {
                @for s in &m.series {
                    li class="panel flex flex-col gap-3 p-4" {
                        div class="flex flex-wrap items-baseline justify-between gap-x-4 gap-y-1" {
                            h2 class="display text-xl font-semibold" { a class="text-ink" href=(format!("/library/series/{}", s.id)) { (s.name) } }
                            p class="flex flex-wrap items-center gap-2 text-xs text-muted" { (series_stats(s)) }
                        }
                        ol class="flex gap-3 overflow-x-auto pb-1" aria-label="Parts" {
                            @for p in &s.parts { (part_cover(p)) }
                        }
                    }
                }
            }
        }
    }
}

#[must_use]
pub fn series_list_screen<'a>(m: &SeriesListModel, url: &'a str) -> Screen<'a> {
    library_screen(&m.frame, url, series_list_content(m), None)
}

// ------------------------------------------------------------------ authors and fandoms

#[must_use]
pub fn groups_content(m: &GroupsModel) -> Markup {
    let f = &m.frame;
    let field = m.kind.field();
    let noun = match m.kind {
        GroupKind::Author => "author",
        GroupKind::Fandom => "fandom",
    };
    html! {
        (tabs(f, m.groups.len() as u64))
        (chips(f))
        @if m.groups.is_empty() {
            (empty_result(f))
        } @else {
            ul class="groups grid gap-3 sm:grid-cols-2 lg:grid-cols-3" {
                @for g in &m.groups {
                    @let next = f.query.with(exact_token(field, &g.value, false), |_| false);
                    li {
                        a class=(format!("panel group flex h-full gap-3 p-3 text-ink no-underline hover:border-ink/25 hover:no-underline hue-{}", hue(&g.label)))
                          href=(library_href(Tab::Books, &next, None, 1)) {
                            span class="w-1.5 shrink-0 self-stretch rounded-full bg-[oklch(0.5_0.11_var(--h))]" aria-hidden="true" {}
                            span class="flex min-w-0 flex-col gap-1" {
                                span class="display truncate text-lg font-semibold group-hover:text-ribbon" { (g.label) }
                                span class="flex flex-wrap gap-x-3 text-xs text-muted" {
                                    span class="tabular-nums" { (g.books) (if g.books == 1 { " book" } else { " books" }) }
                                    span class="tabular-nums" { (short(g.words)) " words" }
                                    @if g.complete > 0 { span class="tabular-nums" { (g.complete) " complete" } }
                                    @if let Some(updated) = g.updated { span { "updated " (day(updated)) } }
                                }
                                span class="line-clamp-2 text-xs text-muted italic" { (g.titles.join(" · ")) }
                                span class="sr-only" { "Show the books of this " (noun) }
                            }
                        }
                    }
                }
            }
        }
    }
}

#[must_use]
pub fn groups_screen<'a>(m: &GroupsModel, url: &'a str) -> Screen<'a> {
    library_screen(&m.frame, url, groups_content(m), None)
}

// -------------------------------------------------------------------- one series

#[derive(Debug)]
pub struct SeriesModel {
    pub now: Timestamp,
    pub site: SiteStatus,
    pub series: SeriesView,
}

fn download_form(url: &str, label: &str, primary: bool) -> Markup {
    html! {
        form method="post" action="/jobs" {
            input type="hidden" name="url" value=(url);
            button class=(if primary { "btn btn-primary" } else { "btn" }) type="submit" { (label) }
        }
    }
}

#[must_use]
pub fn series_content(m: &SeriesModel) -> Markup {
    let s = &m.series;
    let id: SeriesId = s.id;
    let missing = s.missing().count();
    let paragraphs = summary_paragraphs(&s.summary_html);
    html! {
        a class="back text-sm" href=(Tab::Series.path()) { "← Series" }
        article class="flex flex-col gap-6" {
            header class="flex flex-col gap-3" {
                p class="eyebrow" { "Series" }
                h1 class="display text-4xl font-semibold" { (s.name) }
                p class="flex flex-wrap items-center gap-2 text-sm text-muted" { (series_stats(s)) }
                @if !paragraphs.is_empty() {
                    div class="summary max-w-prose text-[0.95rem]" { @for p in &paragraphs { p { (p) } } }
                }
                div class="actions flex flex-wrap gap-2" {
                    a class="btn" href=(id.canonical_url()) target="_blank" rel="noreferrer noopener" { "Open on AO3" }
                    @if missing > 0 {
                        (download_form(&id.canonical_url(), &format!("Download the {missing} missing"), true))
                    } @else if s.listed {
                        (download_form(&id.canonical_url(), "Check for new parts", false))
                    } @else {
                        // Never listed: which parts exist is unknown until AO3's page is read.
                        (download_form(&id.canonical_url(), "Find every part on AO3", true))
                    }
                }
            }
            ol class="parts panel divide-y divide-rule" {
                @for p in &s.parts {
                    li class="flex items-center gap-4 p-3" {
                        div class="w-12 shrink-0" {
                            @if let Some(entry) = &p.entry {
                                span class=(format!("cover mini hue-{}", cover_hue(entry))) aria-hidden="true" {}
                            } @else {
                                span class="cover mini absent" aria-hidden="true" {}
                            }
                        }
                        div class="flex min-w-0 flex-1 flex-col gap-0.5" {
                            span class="text-xs text-muted" { @if let Some(n) = p.part { "Part " (n) } }
                            @if let Some(entry) = &p.entry {
                                a class="display truncate font-semibold" href=(format!("/library/{}", p.work)) { (entry.title()) }
                                span class="flex flex-wrap items-center gap-1.5 text-xs text-muted" {
                                    (rating(entry)) (progress(entry))
                                    @if let Some(f) = &entry.facts { span { (short(f.words)) " words" } }
                                }
                            } @else {
                                span class="display truncate font-semibold text-muted" { (p.title.as_deref().unwrap_or("A part not in your library")) }
                                span class="text-xs text-muted" { "Not in your library · " a href=(p.work.canonical_url()) target="_blank" rel="noreferrer noopener" { "work " (p.work) } }
                            }
                        }
                        @if p.entry.is_none() { (download_form(&p.work.canonical_url(), "Download", false)) }
                    }
                }
            }
            (danger_zone(
                &format!("/series/{id}/delete"),
                "Delete the series and its books",
                "Stops following the series, cancels its queued downloads and removes its books, except those that also belong to another series you follow.",
                &html! { input type="hidden" name="back" value="library"; },
            ))
        }
    }
}

#[must_use]
pub fn series_screen<'a>(m: &'a SeriesModel, url: &'a str) -> Screen<'a> {
    Screen {
        title: &m.series.name,
        nav: Nav::Library,
        url,
        job: None,
        site: m.site,
        now: m.now,
        above: bar(m.site, m.now, None),
        content: series_content(m),
        partials: Vec::new(),
    }
}

// ------------------------------------------------------------------------- one book

#[derive(Debug)]
pub struct BookModel {
    pub now: Timestamp,
    pub site: SiteStatus,
    pub entry: LibraryEntry,
    /// Everything recorded about the work (all its tags, its chapters).
    pub state: Option<WorkState>,
}

const TAG_SECTIONS: [(TagKind, &str, TextField); 6] = [
    (TagKind::Fandom, "Fandoms", TextField::Fandom),
    (TagKind::Warning, "Warnings", TextField::Warning),
    (TagKind::Category, "Categories", TextField::Tag),
    (
        TagKind::Relationship,
        "Relationships",
        TextField::Relationship,
    ),
    (TagKind::Character, "Characters", TextField::Character),
    (TagKind::Freeform, "Tags", TextField::Tag),
];

fn stat(label: &str, value: Option<String>) -> Markup {
    html! {
        @if let Some(value) = value {
            div class="flex flex-col" { dt class="eyebrow" { (label) } dd class="tabular-nums" { (value) } }
        }
    }
}

/// Cover, title, byline, badges and the two actions.
fn work_head(entry: &LibraryEntry) -> Markup {
    let work = entry.book.work;
    html! {
        header class="work-head flex flex-col gap-5 sm:flex-row sm:items-start" {
            (cover(entry, true))
            div class="work-id flex min-w-0 flex-col gap-2" {
                h1 class="display text-3xl leading-tight font-semibold sm:text-4xl" { (entry.title()) }
                @if !entry.authors.is_empty() {
                    p class="by text-lg" {
                        "by "
                        @for (i, a) in entry.authors.iter().enumerate() {
                            @if i > 0 { ", " }
                            a href=(exact_href(TextField::Author, &a.user)) { (a.pseud) }
                        }
                    }
                } @else if let Some(by) = byline(entry) {
                    p class="by text-lg" { "by " (by) }
                }
                p class="meta flex flex-wrap items-center gap-2 text-sm text-muted" {
                    (rating(entry))
                    (progress(entry))
                    @if entry.facts.as_ref().is_some_and(|f| f.restricted) {
                        span class="lock chip tone-muted" title="Visible to logged-in AO3 users only" { "Restricted" }
                    }
                }
                div class="actions mt-2 flex flex-wrap gap-2" {
                    a class="btn" href=(work.canonical_url()) target="_blank" rel="noreferrer noopener" { "Open on AO3" }
                    (download_form(&work.canonical_url(), "Check for updates", true))
                    // Shown by `androidOnly` on Android, where the Fanfold → Moon+ bridge app
                    // opens the synced copy; hidden everywhere else and without scripts.
                    a class="btn" href=(moon_reader_href(&entry.book.file_name)) hidden
                        x-data="androidOnly" x-bind:hidden="hidden" { "Open in Moon+ Reader" }
                }
            }
        }
    }
}

/// A link the Fanfold → Moon+ bridge app (`fanfold-moonbridge`) handles: it joins the library
/// path with the device's library folder and opens that file in Moon+ Reader. Spaces are
/// `%20`, not `+`, so the value survives any URI decoder.
fn moon_reader_href(library_path: &str) -> String {
    format!(
        "fanfold://open?path={}",
        form_encode(library_path).replace('+', "%20")
    )
}

/// Every current tag, by kind, each a link to the books that have it; and the chapters.
fn work_tags(state: &WorkState) -> Markup {
    html! {
        @for (kind, heading, field) in TAG_SECTIONS {
            @let names: Vec<&str> = state.tags.iter().filter(|t| t.kind == kind).map(|t| t.name.as_str()).collect();
            @if !names.is_empty() {
                section class="tags flex flex-col gap-2" {
                    h2 class="eyebrow" { (heading) }
                    ul class="flex flex-wrap gap-1.5" {
                        @for name in names {
                            li { a class="inline-block rounded-md border border-rule bg-sheet px-2 py-0.5 text-sm text-ink no-underline hover:border-ribbon hover:text-ribbon hover:no-underline" href=(exact_href(field, name)) { (name) } }
                        }
                    }
                }
            }
        }
        @if !state.chapters.is_empty() {
            details class="chapters panel px-4 py-2" {
                summary class="cursor-pointer text-sm font-medium select-none" { "Chapters (" (state.chapters.len()) ")" }
                ol class="mt-2 divide-y divide-rule text-sm" {
                    @for ch in &state.chapters {
                        li class="flex justify-between gap-4 py-1.5" {
                            span { (ch.title) }
                            @if let Some(posted) = ch.posted_on { span class="shrink-0 text-muted" { (day(posted)) } }
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
        a class="back text-sm" href="/library" { "← Library" }
        article class="work flex flex-col gap-8" {
            (work_head(entry))

            @if let Some(f) = facts {
                @let paragraphs = summary_paragraphs(&f.summary_html);
                @if !paragraphs.is_empty() {
                    section class="summary max-w-prose border-l-2 border-rule pl-4 text-[0.95rem]" {
                        h2 class="sr-only" { "Summary" }
                        @for p in &paragraphs { p { (p) } }
                    }
                }
            } @else {
                p class="empty" { "This work has not been checked on AO3 yet, so only its file is known. “Check for updates” fetches the rest." }
            }

            dl class="stats grid grid-cols-2 gap-4 sm:grid-cols-4 lg:grid-cols-5" {
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
                section class="work-series flex flex-col gap-2" {
                    h2 class="eyebrow" { "Series" }
                    ul class="flex flex-col gap-1" {
                        @for s in &entry.series {
                            li {
                                @if let Some(part) = s.part { "Part " (part) " of " }
                                a class="display font-semibold" href=(format!("/library/series/{}", s.id)) { (s.name) }
                            }
                        }
                    }
                }
            }

            @if let Some(state) = &m.state { (work_tags(state)) }

            section class="file flex flex-col gap-1 text-sm" {
                h2 class="eyebrow" { "In your library" }
                p class="mono break-all" { (entry.book.file_name) }
                p class="soft" {
                    "Saved " (when(m.now, entry.book.updated_at))
                    @if let Some(checked) = entry.last_checked { " · checked on AO3 " (when(m.now, checked)) }
                }
            }

            (danger_zone(
                &format!("/works/{work}/delete"),
                "Delete the book",
                "Removes the EPUB from your library and cancels queued downloads of it. Its history is kept; a new download (or an AO3 email) brings it back.",
                &html! { input type="hidden" name="back" value="library"; },
            ))
        }
    }
}

#[must_use]
pub fn book_screen<'a>(m: &'a BookModel, url: &'a str) -> Screen<'a> {
    Screen {
        title: m.entry.title(),
        nav: Nav::Library,
        url,
        job: None,
        site: m.site,
        now: m.now,
        above: bar(m.site, m.now, None),
        content: book_content(m),
        partials: Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn library_links_leave_defaults_out() {
        assert_eq!(
            library_href(Tab::Books, &SearchQuery::default(), None, 1),
            "/library"
        );
        assert_eq!(
            library_href(
                Tab::Books,
                &SearchQuery::parse("ada & is:complete"),
                Some(LibrarySort::Words),
                2
            ),
            "/library?q=ada+%26+is%3Acomplete&sort=words&page=2"
        );
        assert_eq!(
            library_href(Tab::Series, &SearchQuery::parse("x"), None, 1),
            "/library/series?q=x"
        );
        assert_eq!(form_encode("é/?"), "%C3%A9%2F%3F");
    }

    #[test]
    fn moon_reader_links_carry_the_library_path() {
        assert_eq!(
            moon_reader_href("single/quill+ink/The Keeper 10000001.epub"),
            "fanfold://open?path=single%2Fquill%2Bink%2FThe%20Keeper%2010000001.epub"
        );
    }

    #[test]
    fn toggling_a_rating_keeps_the_others() {
        let q = SearchQuery::parse("keeper rating:e");
        assert_eq!(
            with_rating_toggled(&q, Rating::Mature).render().trim_end(),
            "keeper rating:m,e"
        );
        assert_eq!(
            with_rating_toggled(&q, Rating::Explicit)
                .render()
                .trim_end(),
            "keeper"
        );
    }

    #[test]
    fn the_completion_filter_is_one_token() {
        let q = SearchQuery::parse("keeper is:wip");
        assert_eq!(status_of_query(&q), Some(Flag::InProgress));
        assert_eq!(
            query_text(&with_status(&q, Some(Flag::Complete))),
            "keeper is:complete"
        );
        assert_eq!(query_text(&with_status(&q, None)), "keeper");
    }
}
