-- Channel archive sync.
--
-- Replaces the previous two-table per-channel cache (`feed_sources` +
-- `feed_source_items`, capped at ~20 newest videos per source) with a
-- unified storage model:
--
--   - `channel_videos` stores the full archive of uploads per channel,
--     written to by RSS (cheap, anti-bot-safe), the InnerTube sidecar
--     fallback (on RSS failure), and a new yt-dlp `--flat-playlist`
--     backfill loop (monthly per channel). The 20-item-per-source cap
--     is removed; the New Videos feed query controls how many to
--     surface via an explicit LIMIT.
--
--   - `channel_sync_state` consolidates the freshness-tier state that
--     used to live on `feed_sources` (rss_*, sidecar_*) with the new
--     backfill-tier state (backfill_*), plus channel header metadata
--     (title, thumbnail, description) used by GET /api/channels/:id
--     so that route no longer has to hit the discovery sidecar.
--
-- See plans/1779554340536-channel-archive-sync.md for the full design.

------------------------------------------------------------------------
-- 1. channel_videos: unified per-channel video archive.
------------------------------------------------------------------------

CREATE TABLE channel_videos (
    channel_id      TEXT    NOT NULL,
    video_id        TEXT    NOT NULL,
    title           TEXT    NOT NULL,
    channel_title   TEXT,                       -- denormalised; matches feed_source_items convention
    published_at    INTEGER,                    -- unix seconds, may be approximate
    published_raw   TEXT,                       -- raw upload_date / RSS <published> as-given
    duration_s      INTEGER,                    -- nullable; yt-dlp may supply, RSS does not
    view_count      INTEGER,                    -- nullable; yt-dlp may supply, RSS does not
    thumbnail_url   TEXT,                       -- RSS-supplied or derived (i.ytimg.com/vi/<id>/hqdefault.jpg)
    first_seen_at   INTEGER NOT NULL,           -- set on insert, never updated
    last_seen_at    INTEGER NOT NULL,           -- bumped by every successful sighting (RSS, sidecar, or backfill)
    source          TEXT    NOT NULL            -- most recent writer
                     CHECK (source IN ('rss', 'sidecar', 'backfill')),
    is_deleted      INTEGER NOT NULL DEFAULT 0, -- 1 = backfill reconciliation no longer lists it
    PRIMARY KEY (channel_id, video_id)
);

CREATE INDEX idx_channel_videos_channel_published
    ON channel_videos(channel_id, published_at DESC);
CREATE INDEX idx_channel_videos_last_seen
    ON channel_videos(last_seen_at);

-- Partial index: every read query that filters by `is_deleted = 0`
-- (the New Videos feed, child search, channel browse, up-next, etc.)
-- can use this index instead of scanning + filtering on the full
-- `idx_channel_videos_channel_published`. A partial index is smaller
-- (excludes tombstoned rows entirely) and the planner picks it up
-- automatically when the query's WHERE matches the partial
-- expression. Tombstoned rows are still indexed via the non-partial
-- index above for the admin/diagnostic queries that need them.
CREATE INDEX idx_channel_videos_live_channel_published
    ON channel_videos(channel_id, published_at DESC)
    WHERE is_deleted = 0;

-- Migrate existing feed_source_items rows. fetched_at becomes both
-- first_seen_at and last_seen_at; source defaults to 'rss' since the
-- old refresher populated this table primarily via the RSS path.
INSERT INTO channel_videos
    (channel_id, video_id, title, channel_title, published_at, published_raw,
     thumbnail_url, first_seen_at, last_seen_at, source, is_deleted)
SELECT
    COALESCE(channel_id, source_id),
    video_id, title, channel_title, published_at, published_raw,
    thumbnail_url, fetched_at, fetched_at, 'rss', 0
FROM feed_source_items
WHERE kind = 'channel'
ON CONFLICT(channel_id, video_id) DO NOTHING;

------------------------------------------------------------------------
-- 2. channel_sync_state: consolidates feed_sources + new backfill state
--    + channel header metadata.
------------------------------------------------------------------------

CREATE TABLE channel_sync_state (
    channel_id                       TEXT PRIMARY KEY,

    -- Channel header metadata (served by GET /api/channels/:channelId).
    channel_title                    TEXT,
    channel_thumbnail_url            TEXT,
    description                      TEXT,

    -- Freshness tier (RSS + InnerTube sidecar fallback) — formerly feed_sources columns.
    rss_etag                         TEXT,
    rss_last_modified                TEXT,
    rss_last_polled_at               INTEGER,
    rss_last_success_at              INTEGER,
    rss_last_error                   TEXT,
    rss_consecutive_errors           INTEGER NOT NULL DEFAULT 0,
    rss_next_poll_at                 INTEGER NOT NULL DEFAULT 0,
    last_sidecar_fallback_at         INTEGER,

    -- Backfill tier (yt-dlp --flat-playlist) — new.
    backfill_status                  TEXT NOT NULL DEFAULT 'pending'
                                      CHECK (backfill_status IN ('pending','running','complete','failed','shelved')),
    backfill_last_started_at         INTEGER,
    backfill_last_completed_at       INTEGER,
    backfill_last_attempted_at       INTEGER,
    backfill_next_at                 INTEGER NOT NULL DEFAULT 0,
    backfill_lease_expires_at        INTEGER,
    backfill_last_error              TEXT,
    backfill_consecutive_errors      INTEGER NOT NULL DEFAULT 0,
    backfill_videos_observed_total   INTEGER NOT NULL DEFAULT 0,
    backfill_videos_new_last_run     INTEGER NOT NULL DEFAULT 0,
    backfill_videos_removed_last_run INTEGER NOT NULL DEFAULT 0
);

-- One index per claim_due query path.
CREATE INDEX idx_channel_sync_rss_next      ON channel_sync_state(rss_next_poll_at);
CREATE INDEX idx_channel_sync_backfill_next ON channel_sync_state(backfill_next_at)
    WHERE backfill_status != 'shelved';

-- Migrate feed_sources rows (channels only — kind='channel' was the only
-- value after migration 017). All channels start with backfill_status='pending'
-- and backfill_next_at=0 so the new backfill loop picks them up on next tick.
INSERT INTO channel_sync_state
    (channel_id, channel_title,
     rss_etag, rss_last_modified, rss_last_polled_at, rss_last_success_at,
     rss_last_error, rss_consecutive_errors, rss_next_poll_at,
     last_sidecar_fallback_at,
     backfill_status, backfill_next_at)
SELECT
    source_id, title,
    etag, last_modified, last_polled_at, last_success_at,
    last_error, consecutive_errors, next_poll_at,
    last_sidecar_fallback_at,
    'pending', 0
FROM feed_sources
WHERE kind = 'channel';

-- Backfill channel_thumbnail_url from allowlisted_channels for existing
-- pre-upgrade channels. MAX(...) picks one deterministically when more
-- than one child has the channel allowlisted with potentially-different
-- cached thumbnails.
UPDATE channel_sync_state
SET channel_thumbnail_url = (
    SELECT MAX(channel_thumbnail_url) FROM allowlisted_channels
    WHERE allowlisted_channels.channel_id = channel_sync_state.channel_id
)
WHERE channel_thumbnail_url IS NULL;

------------------------------------------------------------------------
-- 3. Drop the legacy tables. feed_source_items references feed_sources
--    via FK with ON DELETE CASCADE; drop the child first.
------------------------------------------------------------------------

DROP TABLE feed_source_items;
DROP TABLE feed_sources;
