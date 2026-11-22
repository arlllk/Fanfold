-- A failed job that a later job for the same target fixed by succeeding: kept in the
-- history, but no longer counted, listed or retried as a failure.
ALTER TABLE jobs ADD COLUMN resolved_by bigint REFERENCES jobs (id) ON DELETE SET NULL;

UPDATE jobs AS f
SET resolved_by = (
    SELECT min(s.id) FROM jobs AS s
    WHERE s.state = 'succeeded'
      AND s.target_kind = f.target_kind
      AND s.target_id = f.target_id
      AND s.updated_at_ms > f.updated_at_ms
)
WHERE f.state = 'failed';
