-- Backfill `channel_videos.published_at` for rows whose `published_raw`
-- is a compact-form YouTube/InnerTube relative-time string ("2d ago",
-- "11mo ago", "1y ago", "3w ago", ...).
--
-- These rows were ingested by the sidecar fallback in
-- `src/services/feed_refresher.rs::sidecar_item_to_row`. Until the
-- accompanying parser fix landed (`split_compact_token`), compact
-- tokens fell through `parse_relative_to_unix` to `None`, so
-- `published_at` was stored as NULL and the feed/channel-archive
-- ordering degraded to `last_seen_at` via the query-side
-- `COALESCE(published_at, last_seen_at) DESC`.
--
-- All write paths COALESCE existing `published_at` ahead of the
-- incoming value (`feed_cache.rs:262`, `channel_backfill.rs:652`), so
-- the affected rows would self-heal on the next sidecar / backfill
-- run that touches the same video — but for items that have already
-- scrolled past the ~15-item sidecar window, "next run that touches
-- it" is the next *full* channel backfill, defaulting to a 30-day
-- cycle. This migration heals everything in place so the channel
-- archive view is correctly ordered without that wait.
--
-- Safety properties:
--   * Only rows with `published_at IS NULL` are touched, so no
--     accurate RSS-sourced timestamp can be clobbered.
--   * Only rows whose `published_raw` matches a strict
--     "<digits><compact-suffix>[ ago]" shape are touched; anything
--     else is left untouched and will continue to fall back to
--     `last_seen_at` ordering.
--   * Negative / absurd offsets (offset > current time) are filtered
--     out, mirroring `parse_relative_to_unix`'s far-past guard.
--   * sqlx wraps each .sql migration in a single transaction, so the
--     scratch TEMP table and the UPDATEs apply atomically.
--   * Idempotent — every UPDATE filters on `published_at IS NULL`, so
--     re-running this migration after some rows have been healed by
--     other means is a no-op for those rows.

-- 1. Normalise `published_raw` once into a TEMP table keyed by the
--    real primary key (channel_id, video_id). Doing the LOWER / TRIM /
--    prefix-strip / suffix-strip work here keeps the per-unit UPDATEs
--    below readable and avoids re-running string ops seven times.
--
--    The prefixes mirror `parse_relative_to_unix`: "streamed live ",
--    "streamed ", "premiered " are stripped because InnerTube emits
--    them on past-livestream / premiere entries. The suffix " ago"
--    is stripped to leave a bare "<digits><unit>" body.
CREATE TEMP TABLE _compact_published_backfill AS
WITH base AS (
    SELECT
        channel_id,
        video_id,
        LOWER(TRIM(published_raw)) AS s
    FROM channel_videos
    WHERE published_at IS NULL
      AND published_raw IS NOT NULL
),
prefix_stripped AS (
    SELECT
        channel_id,
        video_id,
        CASE
            WHEN s LIKE 'streamed live %' THEN substr(s, 15)
            WHEN s LIKE 'streamed %'     THEN substr(s, 10)
            WHEN s LIKE 'premiered %'    THEN substr(s, 11)
            ELSE s
        END AS s
    FROM base
)
SELECT
    channel_id,
    video_id,
    TRIM(
        CASE WHEN s LIKE '% ago'
             THEN substr(s, 1, length(s) - 4)
             ELSE s
        END
    ) AS body
FROM prefix_stripped;

-- 2. UPDATE per compact suffix, mapping the digit prefix to a seconds
--    offset and writing `unixepoch() - offset` into `published_at`.
--    Order matters: 'mo' (months) must be processed before 'm'
--    (minutes), or "3mo" would be misread as "3m" + stray 'o' and
--    rejected. The LIKE patterns enforce the right last-N chars
--    independently — "3mo" matches `LIKE '%mo'` but not `LIKE '%m'`
--    (it ends in 'o', not 'm') — but we still order defensively
--    in case the LIKE semantics ever drift.
--
--    Each UPDATE's guards:
--      * `published_at IS NULL`             — never overwrite real data
--      * `LIKE '%<suffix>'`                 — fast-pruning suffix match
--      * `length(body) > <suffix-len>`      — non-empty digit prefix
--      * `<prefix> GLOB '[0-9]*'`           — first char is a digit
--                                             (cheap precondition)
--      * `printf('%d', CAST(...)) = <prefix>` — strict pure-digit check
--                                                (rejects "2a3", "0123",
--                                                "-3", "1.5", etc.)
--      * `CAST(...) > 0`                    — non-zero count (a "0d ago"
--                                             would otherwise heal to
--                                             exactly now, which is
--                                             meaningless)
--      * `offset < unixepoch()`             — refuse far-past timestamps,
--                                             mirroring the parser's
--                                             "if offset > now: None"
--                                             guard.

-- Months ('mo' — 30 days, matching `parse_relative_to_unix`'s
-- approximation; same value used by the runtime parser).
UPDATE channel_videos
SET published_at = unixepoch() - (
    CAST(substr(b.body, 1, length(b.body) - 2) AS INTEGER) * 30 * 86400
)
FROM _compact_published_backfill b
WHERE channel_videos.channel_id = b.channel_id
  AND channel_videos.video_id   = b.video_id
  AND channel_videos.published_at IS NULL
  AND b.body LIKE '%mo'
  AND length(b.body) > 2
  AND substr(b.body, 1, length(b.body) - 2) GLOB '[0-9]*'
  AND printf('%d', CAST(substr(b.body, 1, length(b.body) - 2) AS INTEGER))
        = substr(b.body, 1, length(b.body) - 2)
  AND CAST(substr(b.body, 1, length(b.body) - 2) AS INTEGER) > 0
  AND CAST(substr(b.body, 1, length(b.body) - 2) AS INTEGER) * 30 * 86400 < unixepoch();

-- Years ('y' — 365 days, matching the parser).
UPDATE channel_videos
SET published_at = unixepoch() - (
    CAST(substr(b.body, 1, length(b.body) - 1) AS INTEGER) * 365 * 86400
)
FROM _compact_published_backfill b
WHERE channel_videos.channel_id = b.channel_id
  AND channel_videos.video_id   = b.video_id
  AND channel_videos.published_at IS NULL
  AND b.body LIKE '%y'
  AND length(b.body) > 1
  AND substr(b.body, 1, length(b.body) - 1) GLOB '[0-9]*'
  AND printf('%d', CAST(substr(b.body, 1, length(b.body) - 1) AS INTEGER))
        = substr(b.body, 1, length(b.body) - 1)
  AND CAST(substr(b.body, 1, length(b.body) - 1) AS INTEGER) > 0
  AND CAST(substr(b.body, 1, length(b.body) - 1) AS INTEGER) * 365 * 86400 < unixepoch();

-- Weeks ('w').
UPDATE channel_videos
SET published_at = unixepoch() - (
    CAST(substr(b.body, 1, length(b.body) - 1) AS INTEGER) * 7 * 86400
)
FROM _compact_published_backfill b
WHERE channel_videos.channel_id = b.channel_id
  AND channel_videos.video_id   = b.video_id
  AND channel_videos.published_at IS NULL
  AND b.body LIKE '%w'
  AND length(b.body) > 1
  AND substr(b.body, 1, length(b.body) - 1) GLOB '[0-9]*'
  AND printf('%d', CAST(substr(b.body, 1, length(b.body) - 1) AS INTEGER))
        = substr(b.body, 1, length(b.body) - 1)
  AND CAST(substr(b.body, 1, length(b.body) - 1) AS INTEGER) > 0
  AND CAST(substr(b.body, 1, length(b.body) - 1) AS INTEGER) * 7 * 86400 < unixepoch();

-- Days ('d').
UPDATE channel_videos
SET published_at = unixepoch() - (
    CAST(substr(b.body, 1, length(b.body) - 1) AS INTEGER) * 86400
)
FROM _compact_published_backfill b
WHERE channel_videos.channel_id = b.channel_id
  AND channel_videos.video_id   = b.video_id
  AND channel_videos.published_at IS NULL
  AND b.body LIKE '%d'
  AND length(b.body) > 1
  AND substr(b.body, 1, length(b.body) - 1) GLOB '[0-9]*'
  AND printf('%d', CAST(substr(b.body, 1, length(b.body) - 1) AS INTEGER))
        = substr(b.body, 1, length(b.body) - 1)
  AND CAST(substr(b.body, 1, length(b.body) - 1) AS INTEGER) > 0
  AND CAST(substr(b.body, 1, length(b.body) - 1) AS INTEGER) * 86400 < unixepoch();

-- Hours ('h').
UPDATE channel_videos
SET published_at = unixepoch() - (
    CAST(substr(b.body, 1, length(b.body) - 1) AS INTEGER) * 3600
)
FROM _compact_published_backfill b
WHERE channel_videos.channel_id = b.channel_id
  AND channel_videos.video_id   = b.video_id
  AND channel_videos.published_at IS NULL
  AND b.body LIKE '%h'
  AND length(b.body) > 1
  AND substr(b.body, 1, length(b.body) - 1) GLOB '[0-9]*'
  AND printf('%d', CAST(substr(b.body, 1, length(b.body) - 1) AS INTEGER))
        = substr(b.body, 1, length(b.body) - 1)
  AND CAST(substr(b.body, 1, length(b.body) - 1) AS INTEGER) > 0
  AND CAST(substr(b.body, 1, length(b.body) - 1) AS INTEGER) * 3600 < unixepoch();

-- Minutes ('m'). Must run after the 'mo' arm above; the LIKE '%m'
-- pattern matches strings ending in 'm', which "3mo" does *not*
-- (it ends in 'o'), so this arm naturally skips already-healed
-- month rows.
UPDATE channel_videos
SET published_at = unixepoch() - (
    CAST(substr(b.body, 1, length(b.body) - 1) AS INTEGER) * 60
)
FROM _compact_published_backfill b
WHERE channel_videos.channel_id = b.channel_id
  AND channel_videos.video_id   = b.video_id
  AND channel_videos.published_at IS NULL
  AND b.body LIKE '%m'
  AND length(b.body) > 1
  AND substr(b.body, 1, length(b.body) - 1) GLOB '[0-9]*'
  AND printf('%d', CAST(substr(b.body, 1, length(b.body) - 1) AS INTEGER))
        = substr(b.body, 1, length(b.body) - 1)
  AND CAST(substr(b.body, 1, length(b.body) - 1) AS INTEGER) > 0
  AND CAST(substr(b.body, 1, length(b.body) - 1) AS INTEGER) * 60 < unixepoch();

-- Seconds ('s').
UPDATE channel_videos
SET published_at = unixepoch() - (
    CAST(substr(b.body, 1, length(b.body) - 1) AS INTEGER) * 1
)
FROM _compact_published_backfill b
WHERE channel_videos.channel_id = b.channel_id
  AND channel_videos.video_id   = b.video_id
  AND channel_videos.published_at IS NULL
  AND b.body LIKE '%s'
  AND length(b.body) > 1
  AND substr(b.body, 1, length(b.body) - 1) GLOB '[0-9]*'
  AND printf('%d', CAST(substr(b.body, 1, length(b.body) - 1) AS INTEGER))
        = substr(b.body, 1, length(b.body) - 1)
  AND CAST(substr(b.body, 1, length(b.body) - 1) AS INTEGER) > 0
  AND CAST(substr(b.body, 1, length(b.body) - 1) AS INTEGER) < unixepoch();

DROP TABLE _compact_published_backfill;
