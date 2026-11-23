//! [`LibraryCatalog`] on PostgreSQL: the books, each with its work's latest version.
//!
//! The shelf is read whole, in one statement for the books and one each for their authors,
//! tags, series and latest counters. Searching, filtering and ordering it are the
//! application's (`fanfold_app::browse`), so no rule about them lives in SQL.

use std::collections::HashMap;

use async_trait::async_trait;
use fanfold_app::{
    error::RepoError,
    ports::{BookRecord, LibraryCatalog, LibraryEntry, LibraryRevision, SeriesShelf, SeriesWork},
};
use fanfold_domain::{
    metadata::{Author, SeriesPart, Tag, WorkStats},
    work::{SeriesId, WorkId},
};
use sqlx::postgres::PgRow;

use super::{author_from_row, corrupt, facts_from_row, get, series_from_row, tag_from_row};
use crate::{PgStore, backend, ts};

/// Every book, with its work's latest version (NULLs for a work never observed); $1 is one
/// work, or NULL for all of them.
const ENTRIES_SQL: &str = "
    WITH latest AS (
        SELECT DISTINCT ON (work_id)
               work_id, title, byline, summary_html, language, rating, restricted, status,
               to_char(published_on, 'YYYY-MM-DD') AS published_on,
               to_char(updated_on, 'YYYY-MM-DD') AS updated_on,
               chapters_posted, chapters_total, words
        FROM work_versions
        WHERE $1::bigint IS NULL OR work_id = $1
        ORDER BY work_id, id DESC
    )
    SELECT b.work_id, b.file_name, b.title AS book_title, b.updated_at_ms,
           l.title, l.byline, l.summary_html, l.language, l.rating, l.restricted, l.status,
           l.published_on, l.updated_on, l.chapters_posted, l.chapters_total, l.words,
           (l.work_id IS NOT NULL) AS observed,
           w.last_checked_at_ms
    FROM books b
    LEFT JOIN latest l ON l.work_id = b.work_id
    LEFT JOIN works w ON w.work_id = b.work_id
    WHERE $1::bigint IS NULL OR b.work_id = $1
    ORDER BY b.work_id";

/// A summary of everything the shelf shows: any change moves at least one number. Sums
/// rather than maxima, so a deleted book or a re-saved one moves it too. Cheap: one pass
/// over the books, works and followed series, two index lookups.
const REVISION_SQL: &str = "
    SELECT (SELECT count(*) FROM books),
           (SELECT COALESCE(sum(work_id), 0)::bigint FROM books),
           (SELECT COALESCE(sum(updated_at_ms % 1000000007), 0)::bigint FROM books),
           (SELECT COALESCE(max(id), 0) FROM work_versions),
           (SELECT COALESCE(max(id), 0) FROM work_stats),
           (SELECT COALESCE(sum(last_checked_at_ms % 1000000007), 0)::bigint FROM works),
           (SELECT COALESCE(max(id), 0) FROM series_versions),
           (SELECT count(*) FROM series WHERE followed_at_ms IS NOT NULL),
           (SELECT COALESCE(sum(series_id), 0)::bigint FROM series
            WHERE followed_at_ms IS NOT NULL)";

/// Followed series and those a book's work belongs to, with their latest listing.
const SERIES_SQL: &str = "
    WITH ids AS (
        SELECT series_id FROM series WHERE followed_at_ms IS NOT NULL
        UNION
        SELECT ws.series_id FROM work_series ws JOIN books b ON b.work_id = ws.work_id
        WHERE ws.removed_in IS NULL
    ),
    latest AS (
        SELECT DISTINCT ON (series_id) series_id, name, summary_html, complete, works
        FROM series_versions
        ORDER BY series_id, id DESC
    )
    SELECT i.series_id,
           COALESCE(l.name,
                    (SELECT w.name FROM work_series w WHERE w.series_id = i.series_id
                     ORDER BY w.added_in DESC LIMIT 1),
                    '') AS name,
           COALESCE(l.summary_html, '') AS summary_html,
           l.complete,
           COALESCE(l.works, '{}') AS works,
           COALESCE(s.followed_at_ms IS NOT NULL, false) AS followed
    FROM ids i
    LEFT JOIN latest l ON l.series_id = i.series_id
    LEFT JOIN series s ON s.series_id = i.series_id
    ORDER BY i.series_id";

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
        authors: Vec::new(),
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
    /// Every row of `sql` (bound to the works), grouped by work, in the query's order.
    async fn by_work<T>(
        &self,
        sql: &'static str,
        ids: &[i64],
        read: fn(&PgRow) -> Result<T, RepoError>,
    ) -> Result<HashMap<i64, Vec<T>>, RepoError> {
        let mut out: HashMap<i64, Vec<T>> = HashMap::new();
        for row in sqlx::query(sql)
            .bind(ids)
            .fetch_all(&self.pool)
            .await
            .map_err(|e| backend(&e))?
        {
            out.entry(get(&row, "work_id")?)
                .or_default()
                .push(read(&row)?);
        }
        Ok(out)
    }

    /// The books (one, or all), completed with their authors, tags, series and counters.
    async fn entries(&self, work: Option<WorkId>) -> Result<Vec<LibraryEntry>, RepoError> {
        let mut entries: Vec<LibraryEntry> = sqlx::query(ENTRIES_SQL)
            .bind(work.map(WorkId::to_i64))
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
        let mut authors: HashMap<i64, Vec<Author>> = self
            .by_work(
                "SELECT work_id, user_name, pseud FROM work_authors
                 WHERE work_id = ANY($1) AND removed_in IS NULL ORDER BY work_id, position",
                &ids,
                author_from_row,
            )
            .await?;
        let mut tags: HashMap<i64, Vec<Tag>> = self
            .by_work(
                "SELECT work_id, kind, name FROM work_tags
                 WHERE work_id = ANY($1) AND removed_in IS NULL ORDER BY work_id, position",
                &ids,
                tag_from_row,
            )
            .await?;
        let mut series: HashMap<i64, Vec<SeriesPart>> = self
            .by_work(
                "SELECT work_id, series_id, name, part FROM work_series
                 WHERE work_id = ANY($1) AND removed_in IS NULL ORDER BY work_id, position",
                &ids,
                series_from_row,
            )
            .await?;
        let mut stats: HashMap<i64, Vec<WorkStats>> = self
            .by_work(
                "SELECT DISTINCT ON (work_id) work_id, hits, kudos, comments, bookmarks
                 FROM work_stats WHERE work_id = ANY($1)
                 ORDER BY work_id, seen_at_ms DESC, id DESC",
                &ids,
                |row| {
                    Ok(WorkStats {
                        hits: opt_u64(row, "hits")?,
                        kudos: opt_u64(row, "kudos")?,
                        comments: opt_u64(row, "comments")?,
                        bookmarks: opt_u64(row, "bookmarks")?,
                    })
                },
            )
            .await?;
        for entry in &mut entries {
            let id = entry.book.work.to_i64();
            entry.authors = authors.remove(&id).unwrap_or_default();
            entry.tags = tags.remove(&id).unwrap_or_default();
            entry.series = series.remove(&id).unwrap_or_default();
            entry.stats = stats
                .remove(&id)
                .and_then(|s| s.into_iter().next())
                .unwrap_or_default();
        }
        Ok(entries)
    }
}

#[async_trait]
impl LibraryCatalog for PgStore {
    async fn revision(&self) -> Result<LibraryRevision, RepoError> {
        let row = sqlx::query(REVISION_SQL)
            .fetch_one(&self.pool)
            .await
            .map_err(|e| backend(&e))?;
        let parts = (0..9)
            .map(|i| sqlx::Row::try_get::<i64, _>(&row, i).map_err(|e| backend(&e)))
            .collect::<Result<_, _>>()?;
        Ok(LibraryRevision(parts))
    }

    async fn shelf(&self) -> Result<Vec<LibraryEntry>, RepoError> {
        self.entries(None).await
    }

    async fn entry(&self, work: WorkId) -> Result<Option<LibraryEntry>, RepoError> {
        Ok(self.entries(Some(work)).await?.into_iter().next())
    }

    async fn series_shelf(&self) -> Result<Vec<SeriesShelf>, RepoError> {
        let rows = sqlx::query(SERIES_SQL)
            .fetch_all(&self.pool)
            .await
            .map_err(|e| backend(&e))?;
        let listed: Vec<i64> = rows
            .iter()
            .map(|r| get::<Vec<i64>>(r, "works"))
            .collect::<Result<Vec<_>, _>>()?
            .concat();
        let titles: HashMap<i64, String> = sqlx::query(
            "SELECT DISTINCT ON (work_id) work_id, title FROM work_versions
             WHERE work_id = ANY($1) ORDER BY work_id, id DESC",
        )
        .bind(&listed)
        .fetch_all(&self.pool)
        .await
        .map_err(|e| backend(&e))?
        .iter()
        .map(|r| Ok((get(r, "work_id")?, get(r, "title")?)))
        .collect::<Result<_, RepoError>>()?;
        rows.iter()
            .map(|row| {
                let works: Vec<i64> = get(row, "works")?;
                Ok(SeriesShelf {
                    id: SeriesId::from_i64(get(row, "series_id")?)
                        .ok_or_else(|| corrupt("series_id"))?,
                    name: get(row, "name")?,
                    summary_html: get(row, "summary_html")?,
                    complete: get(row, "complete")?,
                    listed: works
                        .into_iter()
                        .map(|w| {
                            Ok(SeriesWork {
                                work: WorkId::from_i64(w).ok_or_else(|| corrupt("series works"))?,
                                title: titles.get(&w).cloned(),
                            })
                        })
                        .collect::<Result<_, RepoError>>()?,
                    followed: get(row, "followed")?,
                })
            })
            .collect()
    }
}
