//! [`MetadataStore`] on PostgreSQL: versions, spans and statistics (migration 0006).
//!
//! Every observation is recorded in one transaction that locks the work's row, so two
//! downloads of one work can never interleave their span changes. What changed is decided
//! by `fanfold_domain::metadata::plan_work_update`, the same code the in-memory store uses.

use std::collections::HashMap;

use async_trait::async_trait;
use fanfold_app::{
    error::RepoError,
    ports::{MetadataStore, Recorded},
};
use fanfold_domain::{
    Timestamp,
    job::JobId,
    metadata::{
        Author, Chapter, Date, FollowSource, SeriesMetadata, SeriesPart, SpanChanges, Tag, TagKind,
        WorkFacts, WorkMetadata, WorkState, plan_work_update,
    },
    timestamp::to_millis,
    work::{SeriesId, WorkId},
};
use sqlx::{PgConnection, Row, postgres::PgRow, types::Json};

use crate::{PgStore, backend};

fn corrupt(what: &str) -> RepoError {
    RepoError::Backend(format!("corrupt metadata row: {what}"))
}

fn get<'r, T>(row: &'r PgRow, column: &str) -> Result<T, RepoError>
where
    T: sqlx::Decode<'r, sqlx::Postgres> + sqlx::Type<sqlx::Postgres>,
{
    row.try_get(column).map_err(|e| backend(&e))
}

fn date(text: Option<String>) -> Result<Option<Date>, RepoError> {
    text.map(|t| t.parse::<Date>().map_err(|_| corrupt("date")))
        .transpose()
}

fn to_i32(n: u32) -> i32 {
    i32::try_from(n).unwrap_or(i32::MAX)
}

fn to_i64(n: u64) -> i64 {
    i64::try_from(n).unwrap_or(i64::MAX)
}

fn to_u32(n: i32, what: &str) -> Result<u32, RepoError> {
    u32::try_from(n).map_err(|_| corrupt(what))
}

fn facts_from_row(row: &PgRow) -> Result<WorkFacts, RepoError> {
    Ok(WorkFacts {
        title: get(row, "title")?,
        byline: get(row, "byline")?,
        summary_html: get(row, "summary_html")?,
        language: get(row, "language")?,
        rating: get(row, "rating")?,
        restricted: get(row, "restricted")?,
        status: get(row, "status")?,
        published_on: date(get(row, "published_on")?)?,
        updated_on: date(get(row, "updated_on")?)?,
        chapters_posted: to_u32(get(row, "chapters_posted")?, "chapters_posted")?,
        chapters_total: get::<Option<i32>>(row, "chapters_total")?
            .map(|n| to_u32(n, "chapters_total"))
            .transpose()?,
        words: u64::try_from(get::<i64>(row, "words")?).map_err(|_| corrupt("words"))?,
    })
}

/// The latest version's facts and the open spans, read inside the caller's transaction.
async fn load_state(conn: &mut PgConnection, work: WorkId) -> Result<Option<WorkState>, RepoError> {
    let id = work.to_i64();
    let Some(row) = sqlx::query(
        "SELECT title, byline, summary_html, language, rating, restricted, status,
                to_char(published_on, 'YYYY-MM-DD') AS published_on,
                to_char(updated_on, 'YYYY-MM-DD') AS updated_on,
                chapters_posted, chapters_total, words
         FROM work_versions WHERE work_id = $1 ORDER BY id DESC LIMIT 1",
    )
    .bind(id)
    .fetch_optional(&mut *conn)
    .await
    .map_err(|e| backend(&e))?
    else {
        return Ok(None);
    };
    let facts = facts_from_row(&row)?;

    let authors = sqlx::query(
        "SELECT user_name, pseud FROM work_authors
         WHERE work_id = $1 AND removed_in IS NULL ORDER BY position",
    )
    .bind(id)
    .fetch_all(&mut *conn)
    .await
    .map_err(|e| backend(&e))?
    .iter()
    .map(|r| {
        Ok(Author {
            user: get(r, "user_name")?,
            pseud: get(r, "pseud")?,
        })
    })
    .collect::<Result<_, RepoError>>()?;

    let tags = sqlx::query(
        "SELECT kind, name FROM work_tags
         WHERE work_id = $1 AND removed_in IS NULL ORDER BY position",
    )
    .bind(id)
    .fetch_all(&mut *conn)
    .await
    .map_err(|e| backend(&e))?
    .iter()
    .map(|r| {
        Ok(Tag {
            kind: get::<String>(r, "kind")?
                .parse::<TagKind>()
                .map_err(|_| corrupt("tag kind"))?,
            name: get(r, "name")?,
        })
    })
    .collect::<Result<_, RepoError>>()?;

    let series = sqlx::query(
        "SELECT series_id, name, part FROM work_series
         WHERE work_id = $1 AND removed_in IS NULL ORDER BY position",
    )
    .bind(id)
    .fetch_all(&mut *conn)
    .await
    .map_err(|e| backend(&e))?
    .iter()
    .map(|r| {
        Ok(SeriesPart {
            id: SeriesId::from_i64(get(r, "series_id")?).ok_or_else(|| corrupt("series_id"))?,
            name: get(r, "name")?,
            part: get::<Option<i32>>(r, "part")?
                .map(|n| to_u32(n, "part"))
                .transpose()?,
        })
    })
    .collect::<Result<_, RepoError>>()?;

    let chapters = sqlx::query(
        "SELECT number, title, ao3_chapter_id, to_char(posted_on, 'YYYY-MM-DD') AS posted_on
         FROM work_chapters WHERE work_id = $1 AND removed_in IS NULL ORDER BY position",
    )
    .bind(id)
    .fetch_all(&mut *conn)
    .await
    .map_err(|e| backend(&e))?
    .iter()
    .map(|r| {
        Ok(Chapter {
            number: to_u32(get(r, "number")?, "chapter number")?,
            title: get(r, "title")?,
            ao3_id: get::<Option<i64>>(r, "ao3_chapter_id")?
                .map(|n| u64::try_from(n).map_err(|_| corrupt("ao3_chapter_id")))
                .transpose()?,
            posted_on: date(get(r, "posted_on")?)?,
        })
    })
    .collect::<Result<_, RepoError>>()?;

    Ok(Some(WorkState {
        facts,
        authors,
        tags,
        series,
        chapters,
    }))
}

async fn apply_authors(
    conn: &mut PgConnection,
    work: i64,
    version: i64,
    changes: &SpanChanges<Author>,
) -> Result<(), RepoError> {
    for gone in &changes.closed {
        sqlx::query(
            "UPDATE work_authors SET removed_in = $2
             WHERE work_id = $1 AND removed_in IS NULL AND user_name = $3 AND pseud = $4",
        )
        .bind(work)
        .bind(version)
        .bind(&gone.user)
        .bind(&gone.pseud)
        .execute(&mut *conn)
        .await
        .map_err(|e| backend(&e))?;
    }
    for (position, added) in &changes.opened {
        sqlx::query(
            "INSERT INTO work_authors (work_id, user_name, pseud, position, added_in)
             VALUES ($1, $2, $3, $4, $5)",
        )
        .bind(work)
        .bind(&added.user)
        .bind(&added.pseud)
        .bind(i64::from(*position))
        .bind(version)
        .execute(&mut *conn)
        .await
        .map_err(|e| backend(&e))?;
    }
    Ok(())
}

async fn apply_tags(
    conn: &mut PgConnection,
    work: i64,
    version: i64,
    changes: &SpanChanges<Tag>,
) -> Result<(), RepoError> {
    for gone in &changes.closed {
        sqlx::query(
            "UPDATE work_tags SET removed_in = $2
             WHERE work_id = $1 AND removed_in IS NULL AND kind = $3 AND name = $4",
        )
        .bind(work)
        .bind(version)
        .bind(gone.kind.as_str())
        .bind(&gone.name)
        .execute(&mut *conn)
        .await
        .map_err(|e| backend(&e))?;
    }
    for (position, added) in &changes.opened {
        sqlx::query(
            "INSERT INTO work_tags (work_id, kind, name, position, added_in)
             VALUES ($1, $2, $3, $4, $5)",
        )
        .bind(work)
        .bind(added.kind.as_str())
        .bind(&added.name)
        .bind(i64::from(*position))
        .bind(version)
        .execute(&mut *conn)
        .await
        .map_err(|e| backend(&e))?;
    }
    Ok(())
}

async fn apply_series(
    conn: &mut PgConnection,
    work: i64,
    version: i64,
    changes: &SpanChanges<SeriesPart>,
) -> Result<(), RepoError> {
    for gone in &changes.closed {
        sqlx::query(
            "UPDATE work_series SET removed_in = $2
             WHERE work_id = $1 AND removed_in IS NULL AND series_id = $3 AND name = $4
               AND part IS NOT DISTINCT FROM $5",
        )
        .bind(work)
        .bind(version)
        .bind(gone.id.to_i64())
        .bind(&gone.name)
        .bind(gone.part.map(to_i32))
        .execute(&mut *conn)
        .await
        .map_err(|e| backend(&e))?;
    }
    for (position, added) in &changes.opened {
        sqlx::query(
            "INSERT INTO work_series (work_id, series_id, name, part, position, added_in)
             VALUES ($1, $2, $3, $4, $5, $6)",
        )
        .bind(work)
        .bind(added.id.to_i64())
        .bind(&added.name)
        .bind(added.part.map(to_i32))
        .bind(i64::from(*position))
        .bind(version)
        .execute(&mut *conn)
        .await
        .map_err(|e| backend(&e))?;
    }
    Ok(())
}

async fn apply_chapters(
    conn: &mut PgConnection,
    work: i64,
    version: i64,
    changes: &SpanChanges<Chapter>,
) -> Result<(), RepoError> {
    for gone in &changes.closed {
        sqlx::query(
            "UPDATE work_chapters SET removed_in = $2
             WHERE work_id = $1 AND removed_in IS NULL AND number = $3 AND title = $4
               AND ao3_chapter_id IS NOT DISTINCT FROM $5
               AND posted_on IS NOT DISTINCT FROM $6::date",
        )
        .bind(work)
        .bind(version)
        .bind(to_i32(gone.number))
        .bind(&gone.title)
        .bind(gone.ao3_id.map(to_i64))
        .bind(gone.posted_on.map(|d| d.to_string()))
        .execute(&mut *conn)
        .await
        .map_err(|e| backend(&e))?;
    }
    for (position, added) in &changes.opened {
        sqlx::query(
            "INSERT INTO work_chapters
                 (work_id, number, title, ao3_chapter_id, posted_on, position, added_in)
             VALUES ($1, $2, $3, $4, $5::date, $6, $7)",
        )
        .bind(work)
        .bind(to_i32(added.number))
        .bind(&added.title)
        .bind(added.ao3_id.map(to_i64))
        .bind(added.posted_on.map(|d| d.to_string()))
        .bind(i64::from(*position))
        .bind(version)
        .execute(&mut *conn)
        .await
        .map_err(|e| backend(&e))?;
    }
    Ok(())
}

#[async_trait]
impl MetadataStore for PgStore {
    async fn record_work(
        &self,
        metadata: &WorkMetadata,
        seen_at: Timestamp,
        job: Option<JobId>,
    ) -> Result<Recorded, RepoError> {
        let work = metadata.work.to_i64();
        let seen_ms = to_millis(seen_at);
        let mut tx = self.pool.begin().await.map_err(|e| backend(&e))?;
        // The upsert also takes the row lock that serialises observations of one work.
        sqlx::query(
            "INSERT INTO works (work_id, first_seen_at_ms, last_checked_at_ms) VALUES ($1, $2, $2)
             ON CONFLICT (work_id) DO UPDATE SET last_checked_at_ms = EXCLUDED.last_checked_at_ms",
        )
        .bind(work)
        .bind(seen_ms)
        .execute(&mut *tx)
        .await
        .map_err(|e| backend(&e))?;
        let stats = metadata.stats;
        sqlx::query(
            "INSERT INTO work_stats (work_id, seen_at_ms, hits, kudos, comments, bookmarks)
             VALUES ($1, $2, $3, $4, $5, $6)",
        )
        .bind(work)
        .bind(seen_ms)
        .bind(stats.hits.map(to_i64))
        .bind(stats.kudos.map(to_i64))
        .bind(stats.comments.map(to_i64))
        .bind(stats.bookmarks.map(to_i64))
        .execute(&mut *tx)
        .await
        .map_err(|e| backend(&e))?;

        let current = load_state(&mut tx, metadata.work).await?;
        let observed = metadata.state();
        let update = plan_work_update(current.as_ref(), &observed);
        if update.new_version {
            let f = &observed.facts;
            let version: i64 = sqlx::query_scalar(
                "INSERT INTO work_versions
                     (work_id, seen_at_ms, job_id, title, byline, summary_html, language, rating,
                      adult, restricted, status, published_on, updated_on, chapters_posted,
                      chapters_total, words, raw)
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12::date, $13::date,
                         $14, $15, $16, $17)
                 RETURNING id",
            )
            .bind(work)
            .bind(seen_ms)
            .bind(job.map(JobId::get))
            .bind(&f.title)
            .bind(&f.byline)
            .bind(&f.summary_html)
            .bind(&f.language)
            .bind(&f.rating)
            .bind(f.adult())
            .bind(f.restricted)
            .bind(&f.status)
            .bind(f.published_on.map(|d| d.to_string()))
            .bind(f.updated_on.map(|d| d.to_string()))
            .bind(to_i32(f.chapters_posted))
            .bind(f.chapters_total.map(to_i32))
            .bind(i64::try_from(f.words).unwrap_or(i64::MAX))
            .bind(Json(&metadata.raw))
            .fetch_one(&mut *tx)
            .await
            .map_err(|e| backend(&e))?;
            apply_authors(&mut tx, work, version, &update.authors).await?;
            apply_tags(&mut tx, work, version, &update.tags).await?;
            apply_series(&mut tx, work, version, &update.series).await?;
            apply_chapters(&mut tx, work, version, &update.chapters).await?;
        }
        tx.commit().await.map_err(|e| backend(&e))?;
        Ok(Recorded {
            new_version: update.new_version,
        })
    }

    async fn record_series(
        &self,
        metadata: &SeriesMetadata,
        seen_at: Timestamp,
        job: Option<JobId>,
    ) -> Result<Recorded, RepoError> {
        let series = metadata.series.to_i64();
        let facts = &metadata.facts;
        let works: Vec<i64> = facts.works.iter().map(|w| w.to_i64()).collect();
        let mut tx = self.pool.begin().await.map_err(|e| backend(&e))?;
        sqlx::query(
            "INSERT INTO series (series_id) VALUES ($1)
             ON CONFLICT (series_id) DO UPDATE SET series_id = EXCLUDED.series_id",
        )
        .bind(series)
        .execute(&mut *tx)
        .await
        .map_err(|e| backend(&e))?;
        let unchanged: bool = sqlx::query_scalar(
            "SELECT COALESCE((
                 SELECT name = $2 AND summary_html = $3 AND complete IS NOT DISTINCT FROM $4
                        AND works = $5
                 FROM series_versions WHERE series_id = $1 ORDER BY id DESC LIMIT 1
             ), false)",
        )
        .bind(series)
        .bind(&facts.name)
        .bind(&facts.summary_html)
        .bind(facts.complete)
        .bind(&works)
        .fetch_one(&mut *tx)
        .await
        .map_err(|e| backend(&e))?;
        if !unchanged {
            sqlx::query(
                "INSERT INTO series_versions
                     (series_id, seen_at_ms, job_id, name, summary_html, complete, works, raw)
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $8)",
            )
            .bind(series)
            .bind(to_millis(seen_at))
            .bind(job.map(JobId::get))
            .bind(&facts.name)
            .bind(&facts.summary_html)
            .bind(facts.complete)
            .bind(&works)
            .bind(Json(&metadata.raw))
            .execute(&mut *tx)
            .await
            .map_err(|e| backend(&e))?;
        }
        tx.commit().await.map_err(|e| backend(&e))?;
        Ok(Recorded {
            new_version: !unchanged,
        })
    }

    async fn work_state(&self, work: WorkId) -> Result<Option<WorkState>, RepoError> {
        let mut conn = self.pool.acquire().await.map_err(|e| backend(&e))?;
        load_state(&mut conn, work).await
    }

    async fn follow_series(
        &self,
        series: SeriesId,
        by: FollowSource,
        at: Timestamp,
    ) -> Result<(), RepoError> {
        sqlx::query(
            "INSERT INTO series (series_id, followed_at_ms, followed_by) VALUES ($1, $2, $3)
             ON CONFLICT (series_id) DO UPDATE SET
                 followed_at_ms = COALESCE(series.followed_at_ms, EXCLUDED.followed_at_ms),
                 followed_by = COALESCE(series.followed_by, EXCLUDED.followed_by)",
        )
        .bind(series.to_i64())
        .bind(to_millis(at))
        .bind(by.as_str())
        .execute(&self.pool)
        .await
        .map(|_| ())
        .map_err(|e| backend(&e))
    }

    async fn followed(&self, candidates: &[SeriesId]) -> Result<Vec<SeriesId>, RepoError> {
        if candidates.is_empty() {
            return Ok(Vec::new());
        }
        let ids: Vec<i64> = candidates.iter().map(|s| s.to_i64()).collect();
        let followed: Vec<i64> = sqlx::query_scalar(
            "SELECT series_id FROM series WHERE series_id = ANY($1) AND followed_at_ms IS NOT NULL",
        )
        .bind(&ids)
        .fetch_all(&self.pool)
        .await
        .map_err(|e| backend(&e))?;
        Ok(candidates
            .iter()
            .copied()
            .filter(|s| followed.contains(&s.to_i64()))
            .collect())
    }

    async fn claim_series_folder(
        &self,
        series: SeriesId,
        proposed: &str,
    ) -> Result<String, RepoError> {
        sqlx::query_scalar(
            "INSERT INTO series (series_id, folder) VALUES ($1, $2)
             ON CONFLICT (series_id) DO UPDATE SET folder = COALESCE(series.folder, EXCLUDED.folder)
             RETURNING folder",
        )
        .bind(series.to_i64())
        .bind(proposed)
        .fetch_one(&self.pool)
        .await
        .map_err(|e| backend(&e))
    }

    async fn series_names(
        &self,
        series: &[SeriesId],
    ) -> Result<HashMap<SeriesId, String>, RepoError> {
        if series.is_empty() {
            return Ok(HashMap::new());
        }
        let ids: Vec<i64> = series.iter().map(|s| s.to_i64()).collect();
        let rows = sqlx::query(
            "SELECT s.id, COALESCE(
                 (SELECT v.name FROM series_versions v WHERE v.series_id = s.id
                  ORDER BY v.id DESC LIMIT 1),
                 (SELECT w.name FROM work_series w WHERE w.series_id = s.id
                  ORDER BY w.added_in DESC LIMIT 1)) AS name
             FROM unnest($1::bigint[]) AS s(id)",
        )
        .bind(&ids)
        .fetch_all(&self.pool)
        .await
        .map_err(|e| backend(&e))?;
        let mut names = HashMap::new();
        for row in &rows {
            let name: Option<String> = get(row, "name")?;
            if let (Some(id), Some(name)) = (SeriesId::from_i64(get(row, "id")?), name) {
                names.insert(id, name);
            }
        }
        Ok(names)
    }
}
