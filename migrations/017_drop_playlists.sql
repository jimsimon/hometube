-- Remove all playlist functionality.

-- Purge playlist rows from feed source cache before tightening CHECK.
DELETE FROM feed_source_items WHERE kind = 'playlist';
DELETE FROM feed_sources WHERE kind = 'playlist';

-- Drop playlist tables.
DROP TABLE IF EXISTS family_playlist_videos;
DROP TABLE IF EXISTS family_playlist_members;
DROP TABLE IF EXISTS family_playlists;
DROP TABLE IF EXISTS child_playlist_videos;
DROP TABLE IF EXISTS child_playlists;
DROP TABLE IF EXISTS allowlisted_playlists;

-- Tighten feed_sources.kind CHECK to channel-only by rebuilding the
-- table. SQLite has no `ALTER TABLE ... ALTER CONSTRAINT`, so we
-- recreate, copy, swap, and recreate the index. `feed_source_items`
-- references feed_sources via FK with ON DELETE CASCADE; the rows
-- survive the rebuild because we drop the parent only after the FK
-- target has been recreated under the same name.
PRAGMA foreign_keys = OFF;

CREATE TABLE feed_sources_new (
    kind                     TEXT    NOT NULL CHECK (kind IN ('channel')),
    source_id                TEXT    NOT NULL,
    title                    TEXT,
    etag                     TEXT,
    last_modified            TEXT,
    last_polled_at           INTEGER,
    last_success_at          INTEGER,
    last_error               TEXT,
    consecutive_errors       INTEGER NOT NULL DEFAULT 0,
    next_poll_at             INTEGER NOT NULL DEFAULT 0,
    last_sidecar_fallback_at INTEGER,
    PRIMARY KEY (kind, source_id)
);

INSERT INTO feed_sources_new
    SELECT kind, source_id, title, etag, last_modified,
           last_polled_at, last_success_at, last_error,
           consecutive_errors, next_poll_at, last_sidecar_fallback_at
      FROM feed_sources;

DROP TABLE feed_sources;
ALTER TABLE feed_sources_new RENAME TO feed_sources;

CREATE INDEX feed_sources_next_poll ON feed_sources (next_poll_at);

PRAGMA foreign_keys = ON;
