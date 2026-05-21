-- Per-like duration metadata.
--
-- Mirrors `hidden_videos.duration_seconds` so the `/child/liked` grid
-- can render a duration badge on each card. Captured at like-time from
-- the player (which already has `metadata.duration_seconds` in scope)
-- so we don't fan out to yt-dlp on every like.
--
-- Nullable for backwards compatibility with rows created before the
-- column existed; the SSR grid simply omits the duration label when
-- the value is missing.

ALTER TABLE video_likes ADD COLUMN duration_seconds INTEGER;

-- Best-effort backfill from the video metadata cache. yt-dlp serializes
-- the duration as a float in seconds; cast to integer to match the
-- column type. Rows whose video isn't in the cache stay NULL and the
-- column populates lazily on the next re-like.
UPDATE video_likes
SET duration_seconds = COALESCE(
        duration_seconds,
        CAST(
            (SELECT json_extract(m.metadata_json, '$.duration')
             FROM video_metadata_cache m
             WHERE m.video_id = video_likes.video_id)
            AS INTEGER
        )
    )
WHERE duration_seconds IS NULL;
