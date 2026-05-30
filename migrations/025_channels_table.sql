-- Promote `channel_sync_state` to the canonical `channels` table.
--
-- Migration 020 added `channel_title`, `channel_thumbnail_url`, and
-- `description` to `channel_sync_state` specifically so `/api/channels/:id`
-- could serve them without round-tripping to the sidecar. We make that
-- table the single source of channel header metadata, rename it for
-- clarity, and strip the duplicates from `allowlisted_channels`.
--
-- The sync-tier columns (`rss_*`, `backfill_*`, `last_sidecar_fallback_at`)
-- stay on `channels`: 1:1 with `channel_id` and the only consumer is the
-- cron-driven refresher/backfiller.

COMMIT;
PRAGMA foreign_keys = OFF;
BEGIN;

-- =========================================================================
-- 1. Backfill any title/thumbnail data from `allowlisted_channels` into
--    `channel_sync_state` for rows missing it, before we drop the
--    duplicates. (Mirror of migration 020's backfill, the other way.)
-- =========================================================================

UPDATE channel_sync_state
SET channel_title = COALESCE(
        channel_title,
        (SELECT MAX(channel_title) FROM allowlisted_channels
          WHERE allowlisted_channels.channel_id = channel_sync_state.channel_id)
    ),
    channel_thumbnail_url = COALESCE(
        channel_thumbnail_url,
        (SELECT MAX(channel_thumbnail_url) FROM allowlisted_channels
          WHERE allowlisted_channels.channel_id = channel_sync_state.channel_id)
    );

-- Some allowlisted channels may not yet have a `channel_sync_state` row
-- (defensively — `add_channel` seeds one, but old data may pre-date that).
--
-- The two `INSERT OR IGNORE` statements below rely on every other
-- column on `channel_sync_state` having either a `NOT NULL DEFAULT`
-- (set in migration 020 — `backfill_status` defaults to `'pending'`,
-- and `backfill_next_at` / `rss_next_poll_at` default to 0) or being
-- nullable. We list the sync-tier defaulted columns explicitly here so
-- that a future migration which adds a new `NOT NULL` column without a
-- default fails loudly at *that* migration's `cargo sqlx prepare` /
-- test run rather than silently breaking migration 025 only when an
-- operator's database happens to have allowlisted channels missing a
-- sync state row.
INSERT OR IGNORE INTO channel_sync_state
    (channel_id, channel_title, channel_thumbnail_url,
     backfill_status, backfill_next_at, rss_next_poll_at)
SELECT DISTINCT ac.channel_id,
       (SELECT MAX(channel_title) FROM allowlisted_channels
         WHERE channel_id = ac.channel_id),
       (SELECT MAX(channel_thumbnail_url) FROM allowlisted_channels
         WHERE channel_id = ac.channel_id),
       'pending', 0, 0
  FROM allowlisted_channels ac;

-- Likewise for channels referenced only via `channel_videos` (e.g. an
-- archive that survived a child unsubscribing). Same explicit-columns
-- rationale as above.
INSERT OR IGNORE INTO channel_sync_state
    (channel_id, backfill_status, backfill_next_at, rss_next_poll_at)
SELECT DISTINCT cv.channel_id, 'pending', 0, 0
  FROM channel_videos cv;

-- =========================================================================
-- 2. Backfill `videos.channel_id` from `channel_videos` where it ended up
--    NULL after migration 024 (per-child tables didn't have channel_id).
--
-- Deterministic pick when a video has been observed under more than one
-- `channel_id` in `channel_videos` (rare yt-dlp / topic-channel
-- duplication — same video listed on an artist channel and a topic
-- channel, for instance). `MAX(channel_id)` gives us a stable winner
-- across re-runs; a bare `LIMIT 1` would be SQLite-implementation-
-- defined and could pick differently on a fresh database vs. a
-- VACUUM'd one.
--
-- Caveat — cross-listed videos: when an artist (UCxxx) and Topic
-- (UCxxx_topic) channel both list the same video, `MAX(channel_id)`
-- lexicographically picks the `_topic` variant. Per-child surfaces
-- (search filter, feed grouping, channel page click-through) will
-- attribute the video to that channel until the next sighting from
-- the artist channel's feed refresh overwrites it via
-- `models::video::upsert`. If product preference is "always the
-- non-Topic channel," replace the subquery here with a CASE that
-- demotes `LIKE '%_topic'` candidates. We don't do that today because
-- (a) self-correcting drift is acceptable for a one-shot migration,
-- and (b) Topic-channel attribution is harmless to access control
-- (the allowlist/block tables key on `video_id`, not channel_id).
-- =========================================================================

-- Cost note: this is a correlated subquery, O(NULL-channel-rows ×
-- channel_videos index lookup). The `WHERE channel_id IS NULL` guard
-- restricts the outer scan to the small subset of `videos` rows that
-- actually need a fix (typically a stub-only history slice), and the
-- inner SELECT uses the `idx_channel_videos_video_id` lookup added in
-- migration 020. For HomeTube-scale installs (single household, lifetime
-- watch history ≤ low six figures) the runtime is sub-second on
-- spinning disk. If a future install scales past that, replace the
-- correlated form with a join-driven UPDATE …  FROM CTE for the same
-- result in a single index scan; we keep the correlated form here
-- because it's simpler and the migration runs once.
UPDATE videos
SET channel_id = (
    SELECT MAX(channel_id) FROM channel_videos
     WHERE channel_videos.video_id = videos.video_id
)
WHERE channel_id IS NULL;

-- =========================================================================
-- 3. Rename channel_sync_state → channels.
-- =========================================================================

ALTER TABLE channel_sync_state RENAME TO channels;

-- Recreate the indexes under their new names. SQLite carries indexes
-- across a RENAME automatically, but the names still reference the old
-- table and that's confusing for diagnostics. Drop and recreate.
DROP INDEX IF EXISTS idx_channel_sync_rss_next;
DROP INDEX IF EXISTS idx_channel_sync_backfill_next;

CREATE INDEX idx_channels_rss_next      ON channels(rss_next_poll_at);
CREATE INDEX idx_channels_backfill_next ON channels(backfill_next_at)
    WHERE backfill_status != 'shelved';

-- =========================================================================
-- 4. Rebuild allowlisted_channels — drop channel_title, channel_thumbnail_url.
-- =========================================================================

CREATE TABLE allowlisted_channels_new (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    child_account_id INTEGER NOT NULL REFERENCES accounts(id),
    channel_id TEXT NOT NULL REFERENCES channels(channel_id),
    added_by INTEGER NOT NULL REFERENCES accounts(id),
    created_at INTEGER NOT NULL DEFAULT (unixepoch()),
    UNIQUE(child_account_id, channel_id)
);
INSERT INTO allowlisted_channels_new (id, child_account_id, channel_id, added_by, created_at)
SELECT id, child_account_id, channel_id, added_by, created_at
  FROM allowlisted_channels
 WHERE channel_id IN (SELECT channel_id FROM channels);
DROP TABLE allowlisted_channels;
ALTER TABLE allowlisted_channels_new RENAME TO allowlisted_channels;

-- =========================================================================
-- 5. Rebuild channel_videos to add the FK on channel_id → channels(channel_id).
--
-- Migration 024 rebuilt channel_videos without this FK because the
-- `channels` table didn't yet exist by that name. Adding it now makes
-- `upsert_channel_videos`'s "parent channels row must exist" invariant
-- enforced at INSERT time rather than relying on caller discipline.
--
-- All three production deletion sites already remove `channel_videos`
-- rows before the corresponding `channels` row (see
-- `feed_cache::gc_orphan_sources`, `allowlist::delete_channel`,
-- `channel_backfill::reconcile_with_allowlist`), so the FK is safe to
-- introduce without ON DELETE CASCADE.
-- =========================================================================

CREATE TABLE channel_videos_new (
    channel_id      TEXT    NOT NULL REFERENCES channels(channel_id),
    video_id        TEXT    NOT NULL REFERENCES videos(video_id),
    published_at    INTEGER,
    published_raw   TEXT,
    view_count      INTEGER,
    first_seen_at   INTEGER NOT NULL,
    last_seen_at    INTEGER NOT NULL,
    source          TEXT    NOT NULL CHECK (source IN ('rss', 'sidecar', 'backfill')),
    is_deleted      INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (channel_id, video_id)
);
-- Filter out any orphan channel_videos rows referencing channel_ids
-- that no longer exist in `channels` (defensive — migration 025's
-- earlier steps `INSERT OR IGNORE` every channel_id observed in
-- channel_videos, so this should be a no-op in practice).
INSERT INTO channel_videos_new (channel_id, video_id, published_at, published_raw,
                                view_count, first_seen_at, last_seen_at, source, is_deleted)
SELECT channel_id, video_id, published_at, published_raw,
       view_count, first_seen_at, last_seen_at, source, is_deleted
  FROM channel_videos
 WHERE channel_id IN (SELECT channel_id FROM channels);
DROP TABLE channel_videos;
ALTER TABLE channel_videos_new RENAME TO channel_videos;

CREATE INDEX idx_channel_videos_channel_published
    ON channel_videos(channel_id, published_at DESC);
CREATE INDEX idx_channel_videos_last_seen
    ON channel_videos(last_seen_at);
CREATE INDEX idx_channel_videos_live_channel_published
    ON channel_videos(channel_id, published_at DESC)
    WHERE is_deleted = 0;
CREATE INDEX idx_channel_videos_video_id
    ON channel_videos(video_id);

COMMIT;

PRAGMA foreign_key_check;

PRAGMA foreign_keys = ON;

BEGIN;
