-- Introduce a shared `videos` reference table.
--
-- Previously, per-video metadata (`video_title`, `video_thumbnail_url`,
-- `channel_id`, `channel_title`, `duration_seconds`) was duplicated across
-- six tables: `allowlisted_videos`, `blocked_videos`, `hidden_videos`,
-- `watch_history`, `video_likes`, `offline_downloads`. Plus
-- `channel_videos.title`/`channel_title`/`thumbnail_url`/`duration_s`.
--
-- Each table refreshed those columns on a different schedule. Most used a
-- `COALESCE`-only "first non-null wins" upsert pattern, which meant
-- YouTube renames never propagated. This migration extracts the per-video
-- metadata into a single `videos` row and rebuilds every per-child table
-- to FK into it.
--
-- channel_title is migrated to live in migration 025 (`channels` table);
-- we drop it here but `videos.channel_id` retains the linkage.
--
-- SQLite requires `PRAGMA foreign_keys = OFF` outside a transaction for
-- safe table rebuilds. Following the established pattern (migrations 002,
-- 003, 005, 016), we break out of sqlx's migration transaction.

COMMIT;
PRAGMA foreign_keys = OFF;
BEGIN;

-- =========================================================================
-- 1. Create the `videos` table.
-- =========================================================================

CREATE TABLE videos (
    video_id            TEXT PRIMARY KEY,
    title               TEXT NOT NULL,
    channel_id          TEXT,
    duration_seconds    INTEGER,
    thumbnail_url       TEXT,
    first_seen_at       INTEGER NOT NULL DEFAULT (unixepoch()),
    last_updated_at     INTEGER NOT NULL DEFAULT (unixepoch())
);

CREATE INDEX idx_videos_channel ON videos(channel_id);

-- =========================================================================
-- 2. Backfill from every existing source.
--
-- For each `video_id`, pick any non-blank text value across all the
-- per-child tables (`MAX` over title/thumbnail) and the earliest known
-- sighting (`MIN` over first-seen timestamps).
--
-- Caveat: SQLite's `MAX` over text is *lexicographic*, not
-- chronological — "ZZZ stale" beats "Aardvark current". That's
-- acceptable for this one-shot backfill because any non-blank seed
-- gets overwritten by the live writers (`models::video::upsert`,
-- `feed_cache::upsert_channel_with_metadata`) the first time a route
-- touches the video. Do NOT propagate this "MAX wins" pattern into
-- future migrations expecting "newest value wins" semantics.
--
-- Long-tail caveat: a video that's allowlisted-but-never-watched and
-- never re-appears in any feed (channel deleted, video private'd
-- after sighting, etc.) has no live writer to correct a stale-but-
-- lex-greater seed. The display will show e.g. "ZZZ stale rename"
-- indefinitely. The blast radius is bounded — access control keys on
-- `video_id`, not title — but operators investigating "why is this
-- title weird" should know the heuristic is one-shot, not eventual.
--
-- A NULL value sorts before any non-null in SQLite's MAX, so MAX
-- picks a real value if one exists. The per-source SELECTs wrap text
-- columns in `NULLIF(TRIM(...), '')` so blank seeds from legacy yt-dlp
-- failures (`title = ''`, `thumbnail_url = ''`) don't get persisted as
-- empty strings — the `video_id` fallback below only fires if EVERY
-- source was blank.
--
-- Placeholder convention: when every source was blank we fall back to
-- the `video_id` itself (matching `src/models/video::upsert_stub`).
-- Live writers refresh `videos.title` via `COALESCE(NULLIF(excluded, ''), stored)`,
-- which only treats *empty* strings as missing — so a sentinel like
-- `'(unknown)'` would stick forever once written. Using `video_id`
-- keeps the placeholder discoverable by downstream consumers
-- (`title == video_id` ⇒ "not yet enriched") without poisoning future
-- refreshes.
--
-- `MAX(NULLIF(duration_seconds, 0))` filters legacy "pre-roll
-- heartbeat" zeroes out of the aggregate before MAX runs. Without
-- the NULLIF, a video whose only sources recorded `duration_seconds
-- = 0` (a known legacy heartbeat bug that wrote 0 before the player
-- knew the real duration) would land 0 in `videos`, and
-- `models::video::upsert`'s conflict clause is
-- `COALESCE(?, videos.duration_seconds)` — a NULL bind from a
-- subsequent heartbeat keeps the stored 0, and the production
-- heartbeat path (src/routes/usage.rs) only sends a positive value,
-- so the 0 can otherwise stick indefinitely. Treating 0 as "missing"
-- here matches the live writer's `filter(|d| *d > 0)` contract.
-- A corrected shorter duration from a heartbeat could still
-- temporarily lose to a stale longer value (MAX wins), but that
-- self-corrects on the next heartbeat tick.
--
-- `MAX(channel_id)` caveat: the same lexicographic-wins property
-- applies as in migration 025 step 2's cross-listed-videos comment.
-- Here we're picking across *sources* (allowlist / blocked / hidden /
-- watch / likes / downloads / channel_videos) for the same video_id,
-- not across channels for one video — but the underlying SQLite
-- semantics are identical. See migration 025 for the full caveat;
-- the live `models::video::upsert` path with its NULLIF refresh
-- overwrites any stale seed on the next sighting.
-- =========================================================================

INSERT INTO videos (video_id, title, channel_id, duration_seconds,
                    thumbnail_url, first_seen_at, last_updated_at)
SELECT video_id,
       COALESCE(MAX(title), video_id)      AS title,
       MAX(channel_id)                     AS channel_id,
       MAX(NULLIF(duration_seconds, 0))    AS duration_seconds,
       MAX(thumbnail_url)                  AS thumbnail_url,
       MIN(first_seen)                     AS first_seen_at,
       MAX(last_seen)                      AS last_updated_at
  FROM (
       -- Explicit `CAST(NULL AS …)` so each UNION branch declares its
       -- column affinity; SQLite would otherwise infer affinity from
       -- the first non-NULL branch, which is fragile if branches are
       -- reordered.
       SELECT video_id,
              NULLIF(TRIM(video_title), '') AS title,
              CAST(NULL AS TEXT)    AS channel_id,
              CAST(NULL AS INTEGER) AS duration_seconds,
              NULLIF(TRIM(video_thumbnail_url), '') AS thumbnail_url,
              created_at AS first_seen, created_at AS last_seen
         FROM allowlisted_videos
       UNION ALL
       SELECT video_id,
              NULLIF(TRIM(video_title), ''),
              CAST(NULL AS TEXT),
              CAST(NULL AS INTEGER),
              CAST(NULL AS TEXT),
              created_at, created_at
         FROM blocked_videos
      UNION ALL
      SELECT video_id,
             NULLIF(TRIM(video_title), ''),
             NULLIF(TRIM(channel_id), ''),
             duration_seconds,
             NULLIF(TRIM(video_thumbnail_url), ''),
             hidden_at, hidden_at
        FROM hidden_videos
      UNION ALL
      SELECT video_id,
             NULLIF(TRIM(video_title), ''),
             NULLIF(TRIM(channel_id), ''),
             duration_seconds,
             NULLIF(TRIM(video_thumbnail_url), ''),
             last_watched_at, last_watched_at
        FROM watch_history
      UNION ALL
      SELECT video_id,
             NULLIF(TRIM(video_title), ''),
             NULLIF(TRIM(channel_id), ''),
             duration_seconds,
             NULLIF(TRIM(video_thumbnail_url), ''),
             liked_at, updated_at
        FROM video_likes
      UNION ALL
       SELECT video_id,
              NULLIF(TRIM(video_title), ''),
              CAST(NULL AS TEXT),
              duration_seconds,
              NULLIF(TRIM(video_thumbnail_url), ''),
              COALESCE(downloaded_at, unixepoch()),
              COALESCE(downloaded_at, unixepoch())
         FROM offline_downloads
      UNION ALL
      SELECT video_id,
             NULLIF(TRIM(title), ''),
             NULLIF(TRIM(channel_id), ''),
             duration_s,
             NULLIF(TRIM(thumbnail_url), ''),
             first_seen_at, last_seen_at
        FROM channel_videos
      UNION ALL
       -- `usage_log` doesn't carry per-video metadata (it stores
       -- (child, video_id, started_at, ended_at, duration_seconds)
       -- only), so we contribute a metadata-less row whose only job
       -- is to make sure the `videos` row exists. Without this,
       -- pre-migration `usage_log` rows whose video was *only* seen
       -- through screen-time accounting (never allowlisted, blocked,
       -- hidden, watched in `watch_history`, liked, downloaded, or
       -- archived under any channel) get dropped silently by the
       -- INNER JOIN in `routes/usage.rs::top_channels` post-
       -- migration. Title falls back to `video_id` via the outer
       -- `COALESCE(MAX(title), video_id)`.
       SELECT video_id,
              CAST(NULL AS TEXT)    AS title,
              CAST(NULL AS TEXT)    AS channel_id,
              CAST(NULL AS INTEGER) AS duration_seconds,
              CAST(NULL AS TEXT)    AS thumbnail_url,
              started_at, COALESCE(ended_at, started_at)
         FROM usage_log
  )
 WHERE video_id IS NOT NULL
 GROUP BY video_id;

-- =========================================================================
-- Data-loss guard.
--
-- Step 2's INSERT filters `video_id IS NOT NULL`, so any corrupt
-- legacy row with a NULL `video_id` would be silently dropped from
-- both `videos` AND every per-child rebuild below (since the rebuilds
-- filter `video_id IN (SELECT video_id FROM videos)`). Migrations are
-- destructive and one-way; surface the loss as a hard failure so an
-- operator can either backfill the missing IDs or explicitly accept
-- the loss before re-running.
--
-- Mechanism: a tiny temp table with a CHECK constraint that fails
-- (aborting the migration transaction) when at least one legacy row
-- has a NULL `video_id`. SQLite's `RAISE(ABORT, ...)` is restricted
-- to trigger programs, so we can't use it directly here; this
-- constraint-violation pattern is the standard workaround. The
-- resulting error message — "CHECK constraint failed:
-- migration_024_null_video_id_guard" — points operators at this
-- table name, which is grep-discoverable in the migration source.
-- =========================================================================

CREATE TEMP TABLE migration_024_null_video_id_guard (
    null_videos_must_not_exist INTEGER NOT NULL
        CHECK (null_videos_must_not_exist = 0)
);

INSERT INTO migration_024_null_video_id_guard (null_videos_must_not_exist)
SELECT COUNT(*) FROM (
    SELECT 1 FROM allowlisted_videos WHERE video_id IS NULL
    UNION ALL
    SELECT 1 FROM blocked_videos     WHERE video_id IS NULL
    UNION ALL
    SELECT 1 FROM hidden_videos      WHERE video_id IS NULL
    UNION ALL
    SELECT 1 FROM watch_history      WHERE video_id IS NULL
    UNION ALL
    SELECT 1 FROM video_likes        WHERE video_id IS NULL
    UNION ALL
    SELECT 1 FROM offline_downloads  WHERE video_id IS NULL
    UNION ALL
    SELECT 1 FROM channel_videos     WHERE video_id IS NULL
    UNION ALL
    SELECT 1 FROM usage_log          WHERE video_id IS NULL
);

DROP TABLE migration_024_null_video_id_guard;

-- =========================================================================
-- 3. Rebuild allowlisted_videos.
-- =========================================================================

CREATE TABLE allowlisted_videos_new (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    child_account_id INTEGER NOT NULL REFERENCES accounts(id),
    video_id TEXT NOT NULL REFERENCES videos(video_id),
    added_by INTEGER NOT NULL REFERENCES accounts(id),
    created_at INTEGER NOT NULL DEFAULT (unixepoch()),
    UNIQUE(child_account_id, video_id)
);
INSERT INTO allowlisted_videos_new (id, child_account_id, video_id, added_by, created_at)
SELECT id, child_account_id, video_id, added_by, created_at
  FROM allowlisted_videos
 WHERE video_id IN (SELECT video_id FROM videos);
DROP TABLE allowlisted_videos;
ALTER TABLE allowlisted_videos_new RENAME TO allowlisted_videos;

-- =========================================================================
-- 4. Rebuild blocked_videos.
-- =========================================================================

CREATE TABLE blocked_videos_new (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    child_account_id INTEGER NOT NULL REFERENCES accounts(id),
    video_id TEXT NOT NULL REFERENCES videos(video_id),
    blocked_by INTEGER NOT NULL REFERENCES accounts(id),
    reason TEXT,
    created_at INTEGER NOT NULL DEFAULT (unixepoch()),
    UNIQUE(child_account_id, video_id)
);
INSERT INTO blocked_videos_new (id, child_account_id, video_id, blocked_by, reason, created_at)
SELECT id, child_account_id, video_id, blocked_by, reason, created_at
  FROM blocked_videos
 WHERE video_id IN (SELECT video_id FROM videos);
DROP TABLE blocked_videos;
ALTER TABLE blocked_videos_new RENAME TO blocked_videos;

-- =========================================================================
-- 5. Rebuild hidden_videos.
-- =========================================================================

CREATE TABLE hidden_videos_new (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    child_account_id INTEGER NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    video_id TEXT NOT NULL REFERENCES videos(video_id),
    hidden_at INTEGER NOT NULL DEFAULT (unixepoch()),
    UNIQUE(child_account_id, video_id)
);
INSERT INTO hidden_videos_new (id, child_account_id, video_id, hidden_at)
SELECT id, child_account_id, video_id, hidden_at
  FROM hidden_videos
 WHERE video_id IN (SELECT video_id FROM videos);
DROP TABLE hidden_videos;
ALTER TABLE hidden_videos_new RENAME TO hidden_videos;
CREATE INDEX idx_hidden_videos_child
    ON hidden_videos(child_account_id, hidden_at DESC);

-- =========================================================================
-- 6. Rebuild watch_history.
-- =========================================================================

CREATE TABLE watch_history_new (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    child_account_id INTEGER NOT NULL REFERENCES accounts(id),
    video_id TEXT NOT NULL REFERENCES videos(video_id),
    progress_seconds INTEGER NOT NULL DEFAULT 0,
    last_watched_at INTEGER NOT NULL DEFAULT (unixepoch()),
    UNIQUE(child_account_id, video_id)
);
INSERT INTO watch_history_new (id, child_account_id, video_id, progress_seconds, last_watched_at)
SELECT id, child_account_id, video_id, progress_seconds, last_watched_at
  FROM watch_history
 WHERE video_id IN (SELECT video_id FROM videos);
DROP TABLE watch_history;
ALTER TABLE watch_history_new RENAME TO watch_history;

-- =========================================================================
-- 7. Rebuild video_likes.
-- =========================================================================

CREATE TABLE video_likes_new (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    child_account_id INTEGER NOT NULL REFERENCES accounts(id),
    video_id TEXT NOT NULL REFERENCES videos(video_id),
    is_deleted INTEGER NOT NULL DEFAULT 0,
    liked_at INTEGER NOT NULL DEFAULT (unixepoch()),
    updated_at INTEGER NOT NULL DEFAULT (unixepoch()),
    UNIQUE(child_account_id, video_id)
);
INSERT INTO video_likes_new (id, child_account_id, video_id, is_deleted, liked_at, updated_at)
SELECT id, child_account_id, video_id, is_deleted, liked_at, updated_at
  FROM video_likes
 WHERE video_id IN (SELECT video_id FROM videos);
DROP TABLE video_likes;
ALTER TABLE video_likes_new RENAME TO video_likes;

-- =========================================================================
-- 8. Rebuild offline_downloads.
-- =========================================================================

CREATE TABLE offline_downloads_new (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    child_account_id INTEGER NOT NULL REFERENCES accounts(id),
    video_id TEXT NOT NULL REFERENCES videos(video_id),
    quality_label TEXT NOT NULL,
    file_size_bytes INTEGER,
    status TEXT NOT NULL DEFAULT 'pending'
        CHECK (status IN ('pending', 'downloading', 'complete', 'failed', 'deleted')),
    downloaded_at INTEGER,
    UNIQUE(child_account_id, video_id, quality_label)
);
INSERT INTO offline_downloads_new (id, child_account_id, video_id, quality_label,
                                   file_size_bytes, status, downloaded_at)
SELECT id, child_account_id, video_id, quality_label,
       file_size_bytes, status, downloaded_at
  FROM offline_downloads
 WHERE video_id IN (SELECT video_id FROM videos);
DROP TABLE offline_downloads;
ALTER TABLE offline_downloads_new RENAME TO offline_downloads;

-- =========================================================================
-- 9. Rebuild channel_videos — drop title/channel_title/thumbnail_url/duration_s.
--
-- The `published_*`, `view_count`, `first_seen_at`, `last_seen_at`,
-- `source`, `is_deleted` columns remain on `channel_videos` because they
-- describe the channel↔video relationship rather than the video itself.
-- =========================================================================

CREATE TABLE channel_videos_new (
    channel_id      TEXT    NOT NULL,
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
INSERT INTO channel_videos_new (channel_id, video_id, published_at, published_raw,
                                view_count, first_seen_at, last_seen_at, source, is_deleted)
SELECT channel_id, video_id, published_at, published_raw,
       view_count, first_seen_at, last_seen_at, source, is_deleted
  FROM channel_videos
 WHERE video_id IN (SELECT video_id FROM videos);
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
