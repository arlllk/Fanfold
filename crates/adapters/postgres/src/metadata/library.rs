//! [`LibraryCatalog`] on PostgreSQL: the books, each with its work's latest version.
//!
//! One statement lists a page (and, with the same `WHERE`, counts the matches), then the
//! page's tags, series and latest counters are read in one query each. The rules (what
//! "complete" means, the orders, how search matches) are `LibraryQuery`'s, and the shared
//! contract tests hold this and the in-memory store to the same results.

use std::collections::HashMap;

use async_trait::async_trait;
use fanfold_app::{
    error::RepoError,
    ports::{
        BookRecord, LibraryCatalog, LibraryEntry, LibraryPage, LibraryQuery, LibrarySort,
        LibraryStatus, LibraryTotals,
    },
};
use fanfold_domain::{
    metadata::{SeriesPart, Tag, TagKind, WorkStats},
    work::{SeriesId, WorkId},
};
use sqlx::postgres::PgRow;

use super::{corrupt, facts_from_row, get, to_u32};
use crate::{PgStore, backend, ts};

// The statements are assembled from these pieces at compile time (`concat!`), so each one is
// a literal: nothing from the request is ever spliced into SQL, it is all bound.

/// Every book, with its work's latest version (NULLs for a work never observed) and the
/// columns the filters and orders use.
macro_rules! entries_cte {
    () => {
        "WITH latest AS (
            SELECT DISTINCT ON (work_id)
                   work_id, title, byline, summary_html, language, rating, restricted, status,
                   to_char(published_on, 'YYYY-MM-DD') AS published_on,
                   to_char(updated_on, 'YYYY-MM-DD') AS updated_on,
                   updated_on AS updated_date,
                   chapters_posted, chapters_total, words
            FROM work_versions
            ORDER BY work_id, id DESC
        ),
        entries AS (
            SELECT b.work_id, b.file_name, b.title AS book_title, b.updated_at_ms,
                   l.title, l.byline, l.summary_html, l.language, l.rating, l.restricted,
                   l.status, l.published_on, l.updated_on, l.updated_date,
                   l.chapters_posted, l.chapters_total, l.words,
                   (l.work_id IS NOT NULL) AS observed,
                   CASE WHEN l.work_id IS NULL THEN NULL
                        ELSE l.chapters_total IS NOT NULL AND l.chapters_total = l.chapters_posted
                   END AS complete,
                   COALESCE(l.title, b.title) AS shown_title,
                   w.last_checked_at_ms
            FROM books b
            LEFT JOIN latest l ON l.work_id = b.work_id
            LEFT JOIN works w ON w.work_id = b.work_id
        )"
    };
}

/// $1 search pattern (NULL: everything), $2 status, $3 one work (NULL: every work).
macro_rules! filter_where {
    () => {
        "WHERE ($1::text IS NULL
               OR e.shown_title ILIKE $1
               OR e.byline ILIKE $1
               OR EXISTS (SELECT 1 FROM work_tags t
                          WHERE t.work_id = e.work_id AND t.removed_in IS NULL
                            AND t.name ILIKE $1)
               OR EXISTS (SELECT 1 FROM work_series s
                          WHERE s.work_id = e.work_id AND s.removed_in IS NULL
                            AND s.name ILIKE $1))
          AND ($2 = 'all'
               OR ($2 = 'complete' AND e.complete)
               OR ($2 = 'in_progress' AND NOT e.complete))
          AND ($3::bigint IS NULL OR e.work_id = $3)"
    };
}

/// One page in an order: $4 limit, $5 offset.
macro_rules! page {
    ($order:literal) => {
        concat!(
            entries_cte!(),
            " SELECT e.* FROM entries e ",
            filter_where!(),
            " ",
            $order,
            " LIMIT $4 OFFSET $5"
        )
    };
}

/// Orders compare `lower(...)` byte-wise (`COLLATE "C"`), as Rust compares strings, so the
/// database's locale cannot change them.
const fn page_sql(sort: LibrarySort) -> &'static str {
    match sort {
        LibrarySort::Updated => {
            page!("ORDER BY e.updated_date DESC NULLS LAST, e.updated_at_ms DESC, e.work_id DESC")
        }
        LibrarySort::Title => page!(r#"ORDER BY lower(e.shown_title) COLLATE "C", e.work_id"#),
        LibrarySort::Author => page!(
            r#"ORDER BY lower(e.byline) COLLATE "C" NULLS LAST, lower(e.shown_title) COLLATE "C", e.work_id"#
        ),
        LibrarySort::Words => page!("ORDER BY e.words DESC NULLS LAST, e.work_id"),
    }
}

const COUNT_SQL: &str = concat!(
    entries_cte!(),
    " SELECT count(*) FROM entries e ",
    filter_where!()
);

const TOTALS_SQL: &str = concat!(
    entries_cte!(),
    " SELECT count(*) AS books,
             COALESCE(sum(words), 0)::bigint AS words,
             count(*) FILTER (WHERE complete) AS complete
      FROM entries"
);

/// `ILIKE` pattern for a substring, with the pattern characters taken literally.
fn contains_pattern(needle: &str) -> String {
    let mut out = String::with_capacity(needle.len() + 2);
    out.push('%');
    for c in needle.chars() {
        if matches!(c, '\\' | '%' | '_') {
            out.push('\\');
        }
        out.push(c);
    }
    out.push('%');
    out
}

fn search_pattern(query: &LibraryQuery) -> Option<String> {
    query
        .search
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(contains_pattern)
}

fn entry_from_row(row: &PgRow) -> Result<LibraryEntry, RepoError> {
    let work = WorkId::from_i64(get(row, "work_id")?).ok_or_else(|| corrupt("work_id"))?;
    let observed: bool = get(row, "observed")?;
    Ok(LibraryEntry {
        book: BookRecord {
            work,
            file_name: get(row, "file_name")?,
            title: get(row, "book_title")?,
            updated_at: ts(get(row, "updated_at_ms")?)?,
        },
        facts: if observed {
            Some(facts_from_row(row)?)
        } else {
            None
        },
        tags: Vec::new(),
        series: Vec::new(),
        stats: WorkStats::default(),
        last_checked: get::<Option<i64>>(row, "last_checked_at_ms")?
            .map(ts)
            .transpose()?,
    })
}

fn opt_u64(row: &PgRow, column: &str) -> Result<Option<u64>, RepoError> {
    get::<Option<i64>>(row, column)?
        .map(|n| u64::try_from(n).map_err(|_| corrupt(column)))
        .transpose()
}

impl PgStore {
    /// One page of entries, completed with their tags, series and counters.
    async fn entries(
        &self,
        query: &LibraryQuery,
        work: Option<WorkId>,
    ) -> Result<Vec<LibraryEntry>, RepoError> {
        let mut entries: Vec<LibraryEntry> = sqlx::query(page_sql(query.sort))
            .bind(search_pattern(query))
            .bind(query.status.as_str())
            .bind(work.map(WorkId::to_i64))
            .bind(i64::from(query.limit))
            .bind(i64::from(query.offset))
            .fetch_all(&self.pool)
            .await
            .map_err(|e| backend(&e))?
            .iter()
            .map(entry_from_row)
            .collect::<Result<_, _>>()?;
        if entries.is_empty() {
            return Ok(entries);
        }
        let ids: Vec<i64> = entries.iter().map(|e| e.book.work.to_i64()).collect();

        let mut tags: HashMap<i64, Vec<Tag>> = HashMap::new();
        for row in sqlx::query(
            "SELECT work_id, kind, name FROM work_tags
             WHERE work_id = ANY($1) AND removed_in IS NULL
               AND kind IN ('fandom', 'relationship', 'warning')
             ORDER BY work_id, position",
        )
        .bind(&ids)
        .fetch_all(&self.pool)
        .await
        .map_err(|e| backend(&e))?
        {
            tags.entry(get(&row, "work_id")?).or_default().push(Tag {
                kind: get::<String>(&row, "kind")?
                    .parse::<TagKind>()
                    .map_err(|_| corrupt("tag kind"))?,
                name: get(&row, "name")?,
            });
        }

        let mut series: HashMap<i64, Vec<SeriesPart>> = HashMap::new();
        for row in sqlx::query(
            "SELECT work_id, series_id, name, part FROM work_series
             WHERE work_id = ANY($1) AND removed_in IS NULL
             ORDER BY work_id, position",
        )
        .bind(&ids)
        .fetch_all(&self.pool)
        .await
        .map_err(|e| backend(&e))?
        {
            series
                .entry(get(&row, "work_id")?)
                .or_default()
                .push(SeriesPart {
                    id: SeriesId::from_i64(get(&row, "series_id")?)
                        .ok_or_else(|| corrupt("series_id"))?,
                    name: get(&row, "name")?,
                    part: get::<Option<i32>>(&row, "part")?
                        .map(|n| to_u32(n, "part"))
                        .transpose()?,
                });
        }

        let mut stats: HashMap<i64, WorkStats> = HashMap::new();
        for row in sqlx::query(
            "SELECT DISTINCT ON (work_id) work_id, hits, kudos, comments, bookmarks
             FROM work_stats WHERE work_id = ANY($1)
             ORDER BY work_id, seen_at_ms DESC, id DESC",
        )
        .bind(&ids)
        .fetch_all(&self.pool)
        .await
        .map_err(|e| backend(&e))?
        {
            stats.insert(
                get(&row, "work_id")?,
                WorkStats {
                    hits: opt_u64(&row, "hits")?,
                    kudos: opt_u64(&row, "kudos")?,
                    comments: opt_u64(&row, "comments")?,
                    bookmarks: opt_u64(&row, "bookmarks")?,
                },
            );
        }

        for entry in &mut entries {
            let id = entry.book.work.to_i64();
            entry.tags = tags.remove(&id).unwrap_or_default();
            entry.series = series.remove(&id).unwrap_or_default();
            entry.stats = stats.remove(&id).unwrap_or_default();
        }
        Ok(entries)
    }
}

#[async_trait]
impl LibraryCatalog for PgStore {
    async fn library(&self, query: &LibraryQuery) -> Result<LibraryPage, RepoError> {
        let entries = self.entries(query, None).await?;
        let matching: i64 = sqlx::query_scalar(COUNT_SQL)
            .bind(search_pattern(query))
            .bind(query.status.as_str())
            .bind(None::<i64>)
            .fetch_one(&self.pool)
            .await
            .map_err(|e| backend(&e))?;
        let totals = sqlx::query(TOTALS_SQL)
            .fetch_one(&self.pool)
            .await
            .map_err(|e| backend(&e))?;
        let count = |column: &str| -> Result<u64, RepoError> {
            u64::try_from(get::<i64>(&totals, column)?).map_err(|_| corrupt(column))
        };
        Ok(LibraryPage {
            entries,
            matching: u64::try_from(matching).map_err(|_| corrupt("count"))?,
            totals: LibraryTotals {
                books: count("books")?,
                words: count("words")?,
                complete: count("complete")?,
            },
        })
    }

    async fn entry(&self, work: WorkId) -> Result<Option<LibraryEntry>, RepoError> {
        let one = LibraryQuery {
            search: None,
            status: LibraryStatus::All,
            sort: LibrarySort::Updated,
            limit: 1,
            offset: 0,
        };
        Ok(self.entries(&one, Some(work)).await?.into_iter().next())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn search_patterns_take_pattern_characters_literally() {
        assert_eq!(contains_pattern("tide"), "%tide%");
        assert_eq!(contains_pattern("100%"), r"%100\%%");
        assert_eq!(contains_pattern("a_b"), r"%a\_b%");
        assert_eq!(contains_pattern(r"c:\x"), r"%c:\\x%");
    }
}
