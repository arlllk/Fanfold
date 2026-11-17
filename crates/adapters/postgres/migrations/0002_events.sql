-- Change notifications for live views. Payloads are tiny ("j:<job id>" and
-- "l:<job id>"): NOTIFY is capped at 8 kB and is not durable, so listeners treat it
-- as a hint to refetch, never as the source of truth.

CREATE FUNCTION fanfold_notify_job() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    PERFORM pg_notify('fanfold_events', 'j:' || NEW.id);
    RETURN NULL;
END
$$;

CREATE TRIGGER jobs_notify AFTER INSERT OR UPDATE ON jobs
    FOR EACH ROW EXECUTE FUNCTION fanfold_notify_job();

CREATE FUNCTION fanfold_notify_log() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    PERFORM pg_notify('fanfold_events', 'l:' || NEW.job_id);
    RETURN NULL;
END
$$;

CREATE TRIGGER job_logs_notify AFTER INSERT ON job_logs
    FOR EACH ROW EXECUTE FUNCTION fanfold_notify_log();
