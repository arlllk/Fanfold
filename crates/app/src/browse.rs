//! Browsing the library: the books a search finds, and the same books by series, author or
//! fandom.
//!
//! The whole library is read once into memory with its search index, and read again only
//! when [`LibraryCatalog::revision`] says something changed, so a search as you type costs
//! one tiny query plus work in memory. The index finds the books (text relevance, filters);
//! everything else (orders, counts, groups) is decided here, the same way for every store.

use std::{
    cmp::{Ordering, Reverse},
    collections::{BTreeMap, HashMap, HashSet},
    sync::Arc,
};

use fanfold_domain::{
    metadata::{Date, TagKind},
    work::{SeriesId, WorkId},
};
use thiserror::Error;
use tokio::sync::{Mutex, RwLock};

use crate::{
    error::{RepoError, SearchError},
    ports::{
        Hit, LibraryCatalog, LibraryEntry, LibraryRevision, SearchIndex, SearchIndexer,
        SeriesShelf, Suggestion,
    },
    search::{Clause, Rating, SearchQuery, TextField, Token},
};

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum BrowseError {
    #[error(transparent)]
    Repo(#[from] RepoError),
    #[error(transparent)]
    Search(#[from] SearchError),
}

/// The order of a list of books. Every order ends with the work id, so it is total.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub enum LibrarySort {
    /// Best match first (only with text to match; otherwise [`LibrarySort::Updated`]).
    Relevance,
    /// AO3's "updated" date, newest first (works without one last), then the newest book.
    #[default]
    Updated,
    /// By title, A to Z (lower-cased, by code point).
    Title,
    /// By byline, A to Z (lower-cased, by code point; none last), then title.
    Author,
    /// Longest first.
    Words,
    /// Most kudos first.
    Kudos,
}

impl LibrarySort {
    pub const ALL: [Self; 6] = [
        Self::Relevance,
        Self::Updated,
        Self::Title,
        Self::Author,
        Self::Words,
        Self::Kudos,
    ];

    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Relevance => "relevance",
            Self::Updated => "updated",
            Self::Title => "title",
            Self::Author => "author",
            Self::Words => "words",
            Self::Kudos => "kudos",
        }
    }

    #[must_use]
    pub fn parse(raw: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|s| s.as_str() == raw)
    }

    /// The order used for `query` when `asked` (or nothing) was asked for: best match when
    /// there is text to match, else what was asked, else most recently updated.
    #[must_use]
    pub fn effective(asked: Option<Self>, query: &SearchQuery) -> Self {
        match asked {
            Some(Self::Relevance) | None if query.has_text() => Self::Relevance,
            Some(Self::Relevance) | None => Self::Updated,
            Some(sort) => sort,
        }
    }
}

/// Totals over the whole library, whatever the search.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct LibraryTotals {
    pub books: u64,
    pub words: u64,
    pub complete: u64,
}

/// How many of the matching books have each value of a filter, for the filter panel.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Facets {
    /// In AO3's order; every rating, with zero counts too.
    pub ratings: Vec<(Rating, u32)>,
    /// Most books first, then by name.
    pub languages: Vec<(String, u32)>,
    /// Most books first, then by name.
    pub warnings: Vec<(String, u32)>,
    pub complete: u32,
    pub in_progress: u32,
    pub restricted: u32,
}

/// One page of the books a search found.
#[derive(Debug, Clone)]
pub struct BookList {
    pub entries: Vec<LibraryEntry>,
    /// Every book that matches (all pages).
    pub matching: u64,
    pub sort: LibrarySort,
    pub facets: Facets,
    pub totals: LibraryTotals,
}

/// One part of a series as the library sees it.
#[derive(Debug, Clone)]
pub struct SeriesPartView {
    /// Its part number, when known.
    pub part: Option<u32>,
    pub work: WorkId,
    /// The book, when it is in the library.
    pub entry: Option<LibraryEntry>,
    /// The work's title when it was ever observed (also for parts not in the library).
    pub title: Option<String>,
    /// Whether the book matches the search (always `false` for a part not in the library).
    pub matches: bool,
}

/// A series, with its parts in order.
#[derive(Debug, Clone)]
pub struct SeriesView {
    pub id: SeriesId,
    pub name: String,
    pub summary_html: String,
    pub complete: Option<bool>,
    pub followed: bool,
    /// In series order: the latest listing's, else by part number.
    pub parts: Vec<SeriesPartView>,
    /// Parts in the library.
    pub in_library: u32,
    /// Books of the series that match the search.
    pub matching: u32,
    /// Words of the books in the library.
    pub words: u64,
    /// The newest AO3 "updated" date of its books.
    pub updated: Option<Date>,
}

impl SeriesView {
    /// The parts that are not in the library.
    pub fn missing(&self) -> impl Iterator<Item = &SeriesPartView> {
        self.parts.iter().filter(|p| p.entry.is_none())
    }
}

/// What the Authors and Fandoms tabs group by.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum GroupKind {
    Author,
    Fandom,
}

impl GroupKind {
    /// The field a group's exact value filters on.
    #[must_use]
    pub const fn field(self) -> TextField {
        match self {
            Self::Author => TextField::Author,
            Self::Fandom => TextField::Fandom,
        }
    }
}

/// An author or a fandom, with its matching books summed up.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Group {
    /// The exact value that filters on it (lower-cased).
    pub value: String,
    pub label: String,
    pub books: u32,
    pub words: u64,
    pub complete: u32,
    pub updated: Option<Date>,
    /// A few titles, most recently updated first.
    pub titles: Vec<String>,
}

/// The library as read at one revision, with its index.
#[derive(Debug)]
struct Snapshot {
    revision: LibraryRevision,
    /// In work order.
    entries: Vec<LibraryEntry>,
    by_work: HashMap<WorkId, usize>,
    series: Vec<SeriesShelf>,
    index: Arc<dyn SearchIndex>,
    totals: LibraryTotals,
}

impl Snapshot {
    fn entry(&self, work: WorkId) -> Option<&LibraryEntry> {
        self.by_work.get(&work).and_then(|i| self.entries.get(*i))
    }

    /// The matching books with their scores, unordered.
    fn matches(&self, query: &SearchQuery) -> Result<Vec<(&LibraryEntry, f32)>, SearchError> {
        Ok(self
            .index
            .find(query)?
            .into_iter()
            .filter_map(|Hit { work, score }| self.entry(work).map(|e| (e, score)))
            .collect())
    }
}

/// Titles listed per group.
const GROUP_TITLES: usize = 3;
/// Suggestions offered at once.
const SUGGESTIONS: usize = 8;

pub struct LibraryBrowser {
    catalog: Arc<dyn LibraryCatalog>,
    indexer: Arc<dyn SearchIndexer>,
    current: RwLock<Option<Arc<Snapshot>>>,
    /// Held while rebuilding, so concurrent requests build once.
    rebuilding: Mutex<()>,
}

impl std::fmt::Debug for LibraryBrowser {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LibraryBrowser").finish_non_exhaustive()
    }
}

impl LibraryBrowser {
    #[must_use]
    pub fn new(catalog: Arc<dyn LibraryCatalog>, indexer: Arc<dyn SearchIndexer>) -> Self {
        Self {
            catalog,
            indexer,
            current: RwLock::new(None),
            rebuilding: Mutex::new(()),
        }
    }

    async fn snapshot(&self) -> Result<Arc<Snapshot>, BrowseError> {
        let revision = self.catalog.revision().await?;
        if let Some(s) = self.fresh(&revision).await {
            return Ok(s);
        }
        let _building = self.rebuilding.lock().await;
        if let Some(s) = self.fresh(&revision).await {
            return Ok(s);
        }
        // Read after the revision: anything that changes meanwhile changes it again, and the
        // next request rebuilds. A snapshot can be newer than its revision, never older.
        let entries = self.catalog.shelf().await?;
        let series = self.catalog.series_shelf().await?;
        let index = self.indexer.build(&entries).await?;
        let totals = LibraryTotals {
            books: entries.len() as u64,
            words: entries
                .iter()
                .filter_map(|e| e.facts.as_ref())
                .map(|f| f.words)
                .sum(),
            complete: entries
                .iter()
                .filter(|e| e.complete() == Some(true))
                .count() as u64,
        };
        let by_work = entries
            .iter()
            .enumerate()
            .map(|(i, e)| (e.book.work, i))
            .collect();
        let snapshot = Arc::new(Snapshot {
            revision,
            entries,
            by_work,
            series,
            index,
            totals,
        });
        tracing::debug!(books = snapshot.entries.len(), "library index rebuilt");
        *self.current.write().await = Some(Arc::clone(&snapshot));
        Ok(snapshot)
    }

    async fn fresh(&self, revision: &LibraryRevision) -> Option<Arc<Snapshot>> {
        self.current
            .read()
            .await
            .as_ref()
            .filter(|s| s.revision == *revision)
            .map(Arc::clone)
    }

    /// One page of the books `query` finds, in `sort` (see [`LibrarySort::effective`]).
    /// `offset` and `limit` count books.
    pub async fn books(
        &self,
        query: &SearchQuery,
        sort: Option<LibrarySort>,
        offset: u32,
        limit: u32,
    ) -> Result<BookList, BrowseError> {
        let snapshot = self.snapshot().await?;
        let mut matches = snapshot.matches(query)?;
        let sort = LibrarySort::effective(sort, query);
        matches.sort_by(|a, b| order(sort, a, b));
        let facets = facets(matches.iter().map(|(e, _)| *e));
        let entries = matches
            .iter()
            .skip(offset as usize)
            .take(limit as usize)
            .map(|(e, _)| (*e).clone())
            .collect();
        Ok(BookList {
            entries,
            matching: matches.len() as u64,
            sort,
            facets,
            totals: snapshot.totals,
        })
    }

    /// Every series with a book that matches `query` (with no query: every series, those
    /// without a book in the library too), most recently updated first.
    pub async fn series(&self, query: &SearchQuery) -> Result<Vec<SeriesView>, BrowseError> {
        let snapshot = self.snapshot().await?;
        let matching: HashSet<WorkId> = snapshot
            .matches(query)?
            .into_iter()
            .map(|(e, _)| e.book.work)
            .collect();
        let mut views: Vec<SeriesView> = series_ids(&snapshot)
            .into_iter()
            .map(|id| series_view(&snapshot, id, |w| matching.contains(&w)))
            .filter(|v| v.matching > 0 || query.is_empty())
            .collect();
        views.sort_by(|a, b| {
            (
                a.updated.is_none(),
                Reverse(a.updated),
                a.name.to_lowercase(),
                a.id,
            )
                .cmp(&(
                    b.updated.is_none(),
                    Reverse(b.updated),
                    b.name.to_lowercase(),
                    b.id,
                ))
        });
        Ok(views)
    }

    /// One series, every part of it (`None` when no book belongs to it and it is not
    /// followed).
    pub async fn one_series(&self, id: SeriesId) -> Result<Option<SeriesView>, BrowseError> {
        let snapshot = self.snapshot().await?;
        Ok(series_ids(&snapshot)
            .contains(&id)
            .then(|| series_view(&snapshot, id, |_| true)))
    }

    /// The authors (or fandoms) of the books `query` finds, most books first.
    pub async fn groups(
        &self,
        kind: GroupKind,
        query: &SearchQuery,
    ) -> Result<Vec<Group>, BrowseError> {
        let snapshot = self.snapshot().await?;
        let mut matches = snapshot.matches(query)?;
        matches.sort_by(|a, b| order(LibrarySort::Updated, a, b));
        let mut groups: BTreeMap<String, Group> = BTreeMap::new();
        for (entry, _) in &matches {
            for (value, label) in group_keys(kind, entry) {
                let group = groups.entry(value.clone()).or_insert_with(|| Group {
                    value,
                    label,
                    books: 0,
                    words: 0,
                    complete: 0,
                    updated: None,
                    titles: Vec::new(),
                });
                group.books += 1;
                group.words += entry.facts.as_ref().map_or(0, |f| f.words);
                group.complete += u32::from(entry.complete() == Some(true));
                group.updated = group
                    .updated
                    .max(entry.facts.as_ref().and_then(|f| f.updated_on));
                if group.titles.len() < GROUP_TITLES {
                    group.titles.push(entry.title().to_owned());
                }
            }
        }
        let mut groups: Vec<Group> = groups.into_values().collect();
        groups.sort_by(|a, b| (Reverse(a.books), &a.value).cmp(&(Reverse(b.books), &b.value)));
        Ok(groups)
    }

    /// What the word being typed could be: authors, series, tags, books... (in its field, if
    /// it has a key). Values the query already filters on are left out.
    pub async fn suggest(&self, query: &SearchQuery) -> Result<Vec<Suggestion>, BrowseError> {
        let Some((field, partial)) = query.partial() else {
            return Ok(Vec::new());
        };
        let snapshot = self.snapshot().await?;
        let taken = |s: &Suggestion| {
            s.kind.field().is_some_and(|field| {
                query.tokens.iter().any(|t| {
                    matches!(&t.clause, Clause::Exact { field: f, value } if *f == field && *value == s.value)
                })
            })
        };
        Ok(snapshot
            .index
            .suggest(field, partial, SUGGESTIONS + query.tokens.len())?
            .into_iter()
            .filter(|s| !taken(s))
            .take(SUGGESTIONS)
            .collect())
    }

    /// The token a picked suggestion adds (`None` for a book, which is opened instead).
    #[must_use]
    pub fn suggestion_token(suggestion: &Suggestion) -> Option<Token> {
        suggestion.kind.field().map(|field| {
            Token::new(Clause::Exact {
                field,
                value: suggestion.value.clone(),
            })
        })
    }
}

/// A count of books, which never comes near `u32::MAX`.
fn count32(n: usize) -> u32 {
    u32::try_from(n).unwrap_or(u32::MAX)
}

fn lower_title(e: &LibraryEntry) -> String {
    e.title().to_lowercase()
}

fn byline(e: &LibraryEntry) -> Option<String> {
    e.facts
        .as_ref()
        .map(|f| f.byline.to_lowercase())
        .filter(|b| !b.is_empty())
}

/// The order of two matches: `sort`, then (for ties) the work id.
fn order(sort: LibrarySort, a: &(&LibraryEntry, f32), b: &(&LibraryEntry, f32)) -> Ordering {
    let (ea, eb) = (a.0, b.0);
    let work = |e: &LibraryEntry| e.book.work;
    let updated = |e: &LibraryEntry| {
        let date = e.facts.as_ref().and_then(|f| f.updated_on);
        (
            date.is_none(),
            Reverse(date),
            Reverse(e.book.updated_at),
            Reverse(e.book.work),
        )
    };
    // `None` last, whichever way the values sort.
    let desc = |x: Option<u64>| (x.is_none(), Reverse(x));
    match sort {
        LibrarySort::Relevance => {
            b.1.total_cmp(&a.1)
                .then_with(|| updated(ea).cmp(&updated(eb)))
        }
        LibrarySort::Updated => updated(ea).cmp(&updated(eb)),
        LibrarySort::Title => lower_title(ea)
            .cmp(&lower_title(eb))
            .then_with(|| work(ea).cmp(&work(eb))),
        LibrarySort::Author => {
            let key = |e: &LibraryEntry| {
                let by = byline(e);
                (by.is_none(), by, lower_title(e), work(e))
            };
            key(ea).cmp(&key(eb))
        }
        LibrarySort::Words => {
            let words = |e: &LibraryEntry| e.facts.as_ref().map(|f| f.words);
            desc(words(ea))
                .cmp(&desc(words(eb)))
                .then_with(|| work(ea).cmp(&work(eb)))
        }
        LibrarySort::Kudos => desc(ea.stats.kudos)
            .cmp(&desc(eb.stats.kudos))
            .then_with(|| work(ea).cmp(&work(eb))),
    }
}

fn facets<'a>(entries: impl Iterator<Item = &'a LibraryEntry>) -> Facets {
    let mut ratings: BTreeMap<Rating, u32> = Rating::ALL.into_iter().map(|r| (r, 0)).collect();
    let mut languages: HashMap<String, u32> = HashMap::new();
    let mut warnings: HashMap<String, u32> = HashMap::new();
    let mut f = Facets::default();
    for e in entries {
        if let Some(facts) = &e.facts {
            *ratings.entry(Rating::from_ao3(&facts.rating)).or_default() += 1;
            if !facts.language.is_empty() {
                *languages.entry(facts.language.clone()).or_default() += 1;
            }
            f.restricted += u32::from(facts.restricted);
        }
        for w in e.tags_of(TagKind::Warning) {
            *warnings.entry(w.to_owned()).or_default() += 1;
        }
        match e.complete() {
            Some(true) => f.complete += 1,
            Some(false) => f.in_progress += 1,
            None => {}
        }
    }
    let by_count = |m: HashMap<String, u32>| {
        let mut v: Vec<(String, u32)> = m.into_iter().collect();
        v.sort_by(|a, b| (Reverse(a.1), &a.0).cmp(&(Reverse(b.1), &b.0)));
        v
    };
    f.ratings = ratings.into_iter().collect();
    f.languages = by_count(languages);
    f.warnings = by_count(warnings);
    f
}

/// What a book is grouped under: (exact value, label) per author or fandom.
fn group_keys(kind: GroupKind, e: &LibraryEntry) -> Vec<(String, String)> {
    let mut keys: Vec<(String, String)> = match kind {
        GroupKind::Author => e
            .authors
            .iter()
            .map(|a| (a.user.to_lowercase(), a.user.clone()))
            .collect(),
        GroupKind::Fandom => e
            .tags_of(TagKind::Fandom)
            .map(|f| (f.to_lowercase(), f.to_owned()))
            .collect(),
    };
    keys.sort();
    keys.dedup_by(|a, b| a.0 == b.0);
    keys
}

/// Every series a book belongs to or that is followed, in id order.
fn series_ids(snapshot: &Snapshot) -> Vec<SeriesId> {
    let mut ids: Vec<SeriesId> = snapshot.series.iter().map(|s| s.id).collect();
    ids.extend(
        snapshot
            .entries
            .iter()
            .flat_map(|e| e.series.iter().map(|p| p.id)),
    );
    ids.sort();
    ids.dedup();
    ids
}

fn series_view(snapshot: &Snapshot, id: SeriesId, matches: impl Fn(WorkId) -> bool) -> SeriesView {
    let shelf = snapshot.series.iter().find(|s| s.id == id);
    // The part numbers and the name the works' own pages give.
    let members: Vec<(&LibraryEntry, Option<u32>, &str)> = snapshot
        .entries
        .iter()
        .filter_map(|e| {
            e.series
                .iter()
                .find(|p| p.id == id)
                .map(|p| (e, p.part, p.name.as_str()))
        })
        .collect();
    let part_of = |work: WorkId| {
        members
            .iter()
            .find(|(e, _, _)| e.book.work == work)
            .and_then(|(_, part, _)| *part)
    };
    let view = |work: WorkId, part: Option<u32>, title: Option<String>| {
        let entry = snapshot.entry(work).cloned();
        SeriesPartView {
            part,
            work,
            matches: entry.is_some() && matches(work),
            title: entry.as_ref().map(|e| e.title().to_owned()).or(title),
            entry,
        }
    };
    let mut parts: Vec<SeriesPartView> = match shelf.filter(|s| !s.listed.is_empty()) {
        // The listing's order, its position as the part number where the work says none.
        Some(listed) => {
            let mut parts: Vec<SeriesPartView> = listed
                .listed
                .iter()
                .zip(1_u32..)
                .map(|(w, n)| view(w.work, part_of(w.work).or(Some(n)), w.title.clone()))
                .collect();
            // Books that say they belong but the last listing did not have yet.
            for (e, part, _) in &members {
                if !parts.iter().any(|p| p.work == e.book.work) {
                    parts.push(view(e.book.work, *part, None));
                }
            }
            parts
        }
        None => members
            .iter()
            .map(|(e, part, _)| view(e.book.work, *part, None))
            .collect(),
    };
    if shelf.is_none_or(|s| s.listed.is_empty()) {
        parts.sort_by_key(|p| (p.part.is_none(), p.part, p.work));
    }
    let books: Vec<&LibraryEntry> = parts.iter().filter_map(|p| p.entry.as_ref()).collect();
    SeriesView {
        id,
        name: shelf
            .map(|s| s.name.clone())
            .filter(|n| !n.is_empty())
            .or_else(|| members.first().map(|(_, _, name)| (*name).to_owned()))
            .unwrap_or_else(|| format!("Series {id}")),
        summary_html: shelf.map_or_else(String::new, |s| s.summary_html.clone()),
        complete: shelf.and_then(|s| s.complete),
        followed: shelf.is_some_and(|s| s.followed),
        in_library: count32(books.len()),
        matching: count32(parts.iter().filter(|p| p.matches).count()),
        words: books
            .iter()
            .filter_map(|e| e.facts.as_ref())
            .map(|f| f.words)
            .sum(),
        updated: books
            .iter()
            .filter_map(|e| e.facts.as_ref().and_then(|f| f.updated_on))
            .max(),
        parts,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_effective_order_prefers_best_match_only_with_text() {
        let text = SearchQuery::parse("keeper");
        let filter = SearchQuery::parse("is:complete");
        assert_eq!(LibrarySort::effective(None, &text), LibrarySort::Relevance);
        assert_eq!(LibrarySort::effective(None, &filter), LibrarySort::Updated);
        assert_eq!(
            LibrarySort::effective(Some(LibrarySort::Relevance), &filter),
            LibrarySort::Updated
        );
        assert_eq!(
            LibrarySort::effective(Some(LibrarySort::Words), &text),
            LibrarySort::Words
        );
        for s in LibrarySort::ALL {
            assert_eq!(LibrarySort::parse(s.as_str()), Some(s));
        }
    }
}
