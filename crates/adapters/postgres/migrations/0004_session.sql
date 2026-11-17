-- The browser session (cookies + User-Agent) the downloader presents to the site, so it can
-- pass bot protection and stay logged in. At most one row; the cookies are secrets.
CREATE TABLE browser_session (
    id            smallint PRIMARY KEY CHECK (id = 1),
    user_agent    text   NOT NULL,
    cookies       jsonb  NOT NULL,
    updated_at_ms bigint NOT NULL
);
