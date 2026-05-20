-- Push-on-schedule new-videos feed: cache tables.
--
-- `feed_sources` is one row per distinct (kind, source_id) currently
-- allowlisted by ANY child. The background refresher reads this table;
-- the allowlist tables determine which children see which source's items.
--
-- `feed_source_items` stores the most recent ~20 items per source. The
-- `/api/feed/new-videos` handler reads from this table directly, joined
-- against the requesting child's allowlist.
--
-- The `kind` column accepts 'playlist' for forward-compatibility, but
-- only 'channel' is exercised by the refresher today.

CREATE TABLE feed_sources (
    kind                TEXT    NOT NULL CHECK (kind IN ('channel','playlist')),
    source_id           TEXT    NOT NULL,
    title               TEXT,
    etag                TEXT,
    last_modified       TEXT,
    last_polled_at      INTEGER,
    last_success_at     INTEGER,
    last_error          TEXT,
    consecutive_errors  INTEGER NOT NULL DEFAULT 0,
    next_poll_at        INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (kind, source_id)
);

CREATE INDEX feed_sources_next_poll ON feed_sources (next_poll_at);

CREATE TABLE feed_source_items (
    kind            TEXT    NOT NULL,
    source_id       TEXT    NOT NULL,
    video_id        TEXT    NOT NULL,
    title           TEXT    NOT NULL,
    channel_id      TEXT,
    channel_title   TEXT,
    thumbnail_url   TEXT,
    published_at    INTEGER,
    published_raw   TEXT,
    fetched_at      INTEGER NOT NULL,
    PRIMARY KEY (kind, source_id, video_id),
    FOREIGN KEY (kind, source_id) REFERENCES feed_sources(kind, source_id) ON DELETE CASCADE
);

CREATE INDEX feed_source_items_published
    ON feed_source_items (kind, source_id, published_at DESC);

CREATE INDEX feed_source_items_video ON feed_source_items (video_id);
