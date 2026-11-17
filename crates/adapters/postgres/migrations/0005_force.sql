-- A forced retry: the next download ignores the stored book and starts from scratch.
-- Cleared when a download succeeds.
ALTER TABLE jobs ADD COLUMN force boolean NOT NULL DEFAULT false;
