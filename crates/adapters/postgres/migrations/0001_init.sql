-- Timestamps are Unix epoch milliseconds (bigint): exact, driver-independent, and the
-- same representation the domain uses. Use to_timestamp(col / 1000.0) for ad-hoc SQL.

CREATE TABLE jobs (
    id                 bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    work_id            bigint  NOT NULL CHECK (work_id > 0),
    source             text    NOT NULL CHECK (source IN ('mail', 'manual')),
    state              text    NOT NULL CHECK (state IN ('queued', 'running', 'succeeded', 'failed', 'cancelled')),
    attempt            integer NOT NULL DEFAULT 0 CHECK (attempt >= 0),
    next_attempt_at_ms bigint  NOT NULL,
    created_at_ms      bigint  NOT NULL,
    updated_at_ms      bigint  NOT NULL,
    last_error         text
);

-- At most one active job per work. This, not application code, is what makes
-- enqueue idempotent under concurrent writers.
CREATE UNIQUE INDEX jobs_one_active_per_work ON jobs (work_id) WHERE state IN ('queued', 'running');
-- Serves the claim query.
CREATE INDEX jobs_due ON jobs (next_attempt_at_ms, id) WHERE state = 'queued';
-- Serves the "recent outcomes" view.
CREATE INDEX jobs_recent ON jobs (updated_at_ms DESC, id DESC);

CREATE TABLE job_logs (
    id      bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    job_id  bigint  NOT NULL REFERENCES jobs (id) ON DELETE CASCADE,
    at_ms   bigint  NOT NULL,
    attempt integer NOT NULL,
    level   text    NOT NULL CHECK (level IN ('info', 'warn', 'error')),
    message text    NOT NULL
);
CREATE INDEX job_logs_by_job ON job_logs (job_id, id);

CREATE TABLE books (
    work_id       bigint PRIMARY KEY CHECK (work_id > 0),
    -- Chosen once and never rewritten: e-readers key reading progress on the path.
    file_name     text   NOT NULL,
    title         text   NOT NULL,
    updated_at_ms bigint NOT NULL
);
