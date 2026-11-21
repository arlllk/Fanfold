-- Every failed try, budget or not: paces the back-off and picks how the next try fetches.
ALTER TABLE jobs ADD COLUMN tries integer NOT NULL DEFAULT 0 CHECK (tries >= 0);
