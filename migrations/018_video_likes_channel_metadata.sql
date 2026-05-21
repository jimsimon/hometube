-- Per-like channel metadata.
--
-- `video_likes` previously stored only the video id + title + thumbnail.
-- That meant the `visible` flag exposed by `/api/likes` (and the SSR
-- grid on `/child/liked`) could only consider direct video-allowlist
-- entries: a like pointing at a video reachable purely via an
-- allowlisted channel was incorrectly marked invisible.
--
-- Capturing channel_id at like-time lets the join against
-- `allowlisted_channels` succeed without re-fetching metadata from
-- yt-dlp. Both columns are nullable so old rows keep working and the
-- field is optional in the POST payload.
--
-- Playlist allowlisting is not addressed here — a video can belong to
-- multiple playlists, which would need a separate join table; channel
-- matching covers the common case.

ALTER TABLE video_likes ADD COLUMN channel_id TEXT;
ALTER TABLE video_likes ADD COLUMN channel_title TEXT;

-- Best-effort backfill from the video metadata cache. Rows whose video
-- isn't (or is no longer) in the cache stay NULL and degrade to the
-- previous video-only visibility behavior until the child re-likes the
-- video, at which point the POST body refreshes both columns. Wrapped
-- in NULLIF so blank JSON values don't shadow a future re-like.
UPDATE video_likes
SET channel_id = COALESCE(
        channel_id,
        NULLIF(
            (SELECT json_extract(m.metadata_json, '$.channel_id')
             FROM video_metadata_cache m
             WHERE m.video_id = video_likes.video_id),
            ''
        )
    ),
    channel_title = COALESCE(
        channel_title,
        NULLIF(
            (SELECT json_extract(m.metadata_json, '$.channel_title')
             FROM video_metadata_cache m
             WHERE m.video_id = video_likes.video_id),
            ''
        )
    )
WHERE channel_id IS NULL OR channel_title IS NULL;
