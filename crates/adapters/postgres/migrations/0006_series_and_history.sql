-- Series support and the history of what AO3 says about works and series.
--
-- The library starts afresh: books are filed under series/ and single/ from their next
-- download on. Their old flat files are left on disk, untouched; remove them by hand.

-- A job fetches a work (download it) or a series (list it and queue its works).
ALTER TABLE jobs RENAME COLUMN work_id TO target_id;
ALTER TABLE jobs
    ADD COLUMN target_kind text NOT NULL DEFAULT 'work' CHECK (target_kind IN ('work', 'series'));
DROP INDEX jobs_one_active_per_work;
CREATE UNIQUE INDEX jobs_one_active_per_target ON jobs (target_kind, target_id)
    WHERE state IN ('queued', 'running');

-- file_name is now a path relative to the library: series/<folder>/<file> or
-- single/<byline>/<file>. Still chosen once and never rewritten.
DELETE FROM books;

-- ---------------------------------------------------------------- work history
--
-- A version is written only when something about the work changed; list items are spans
-- (added_in .. removed_in, both work_versions ids) so "which update added this tag?" is a
-- lookup. Statistics change on every visit and live in work_stats.

CREATE TABLE works (
    work_id            bigint PRIMARY KEY CHECK (work_id > 0),
    first_seen_at_ms   bigint NOT NULL,
    last_checked_at_ms bigint NOT NULL
);

CREATE TABLE work_versions (
    id              bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    work_id         bigint  NOT NULL REFERENCES works,
    seen_at_ms      bigint  NOT NULL,
    job_id          bigint  REFERENCES jobs ON DELETE SET NULL,
    title           text    NOT NULL,
    byline          text    NOT NULL,
    summary_html    text    NOT NULL,
    language        text    NOT NULL,
    rating          text    NOT NULL,
    -- AO3's adult-content gate: Mature, Explicit or Not Rated.
    adult           boolean NOT NULL,
    -- Visible to logged-in users only.
    restricted      boolean NOT NULL,
    status          text    NOT NULL,
    published_on    date,
    updated_on      date,
    chapters_posted integer NOT NULL CHECK (chapters_posted >= 0),
    -- NULL when AO3 shows "?".
    chapters_total  integer CHECK (chapters_total >= 0),
    words           bigint  NOT NULL CHECK (words >= 0),
    -- The downloader's full answer, kept to re-read later.
    raw             jsonb   NOT NULL
);
CREATE INDEX work_versions_by_work ON work_versions (work_id, id);

CREATE TABLE work_authors (
    work_id    bigint  NOT NULL REFERENCES works,
    user_name  text    NOT NULL,
    pseud      text    NOT NULL,
    position   integer NOT NULL,
    added_in   bigint  NOT NULL REFERENCES work_versions,
    removed_in bigint  REFERENCES work_versions
);
CREATE INDEX work_authors_open ON work_authors (work_id) WHERE removed_in IS NULL;

CREATE TABLE work_tags (
    work_id    bigint  NOT NULL REFERENCES works,
    kind       text    NOT NULL
        CHECK (kind IN ('fandom', 'category', 'warning', 'relationship', 'character', 'freeform')),
    name       text    NOT NULL,
    position   integer NOT NULL,
    added_in   bigint  NOT NULL REFERENCES work_versions,
    removed_in bigint  REFERENCES work_versions
);
CREATE UNIQUE INDEX work_tags_open ON work_tags (work_id, kind, name) WHERE removed_in IS NULL;
CREATE INDEX work_tags_by_name ON work_tags (kind, name);

CREATE TABLE work_series (
    work_id    bigint  NOT NULL REFERENCES works,
    series_id  bigint  NOT NULL CHECK (series_id > 0),
    -- The series name as the work's page showed it.
    name       text    NOT NULL,
    part       integer,
    position   integer NOT NULL,
    added_in   bigint  NOT NULL REFERENCES work_versions,
    removed_in bigint  REFERENCES work_versions
);
CREATE INDEX work_series_open ON work_series (work_id) WHERE removed_in IS NULL;
CREATE INDEX work_series_by_series ON work_series (series_id);

CREATE TABLE work_chapters (
    work_id        bigint  NOT NULL REFERENCES works,
    number         integer NOT NULL,
    title          text    NOT NULL,
    ao3_chapter_id bigint,
    posted_on      date,
    position       integer NOT NULL,
    added_in       bigint  NOT NULL REFERENCES work_versions,
    removed_in     bigint  REFERENCES work_versions
);
CREATE INDEX work_chapters_open ON work_chapters (work_id) WHERE removed_in IS NULL;

CREATE TABLE work_stats (
    id         bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    work_id    bigint NOT NULL REFERENCES works,
    seen_at_ms bigint NOT NULL,
    hits       bigint,
    kudos      bigint,
    comments   bigint,
    bookmarks  bigint
);
CREATE INDEX work_stats_by_work ON work_stats (work_id, seen_at_ms);

-- ---------------------------------------------------------------------- series

CREATE TABLE series (
    series_id      bigint PRIMARY KEY CHECK (series_id > 0),
    -- Set once, by the first follow: 'manual' (its link was added) or 'mail'.
    followed_at_ms bigint,
    followed_by    text CHECK (followed_by IN ('manual', 'mail')),
    -- Its folder under series/, chosen once.
    folder         text
);

CREATE TABLE series_versions (
    id           bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    series_id    bigint   NOT NULL REFERENCES series,
    seen_at_ms   bigint   NOT NULL,
    job_id       bigint   REFERENCES jobs ON DELETE SET NULL,
    name         text     NOT NULL,
    summary_html text     NOT NULL,
    complete     boolean,
    -- The works, in series order.
    works        bigint[] NOT NULL,
    raw          jsonb    NOT NULL
);
CREATE INDEX series_versions_by_series ON series_versions (series_id, id);

-- ---------------------------------------------------- views for ad-hoc analysis

-- Every version of every work, with readable times.
CREATE VIEW work_timeline AS
SELECT v.work_id, v.id AS version, to_timestamp(v.seen_at_ms / 1000.0) AS seen_at,
       v.title, v.byline, v.rating, v.adult, v.restricted, v.status,
       v.published_on, v.updated_on, v.chapters_posted, v.chapters_total, v.words
FROM work_versions v;

-- When each tag was added to and removed from each work, with the work's state then.
CREATE VIEW work_tag_timeline AS
SELECT t.work_id, t.kind, t.name,
       to_timestamp(a.seen_at_ms / 1000.0) AS added_seen_at, a.updated_on AS added_with_update_of,
       a.chapters_posted AS chapters_when_added,
       to_timestamp(r.seen_at_ms / 1000.0) AS removed_seen_at, r.updated_on AS removed_with_update_of
FROM work_tags t
JOIN work_versions a ON a.id = t.added_in
LEFT JOIN work_versions r ON r.id = t.removed_in;
