-- Resume tokens for external feeds (e.g. the JMAP mail state), so a restart continues
-- where it stopped instead of replaying a mailbox.
CREATE TABLE cursors (
    name  text PRIMARY KEY,
    value text NOT NULL
);
