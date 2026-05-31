-- Centralise the "video publish date" resolution into one view.
--
-- A video can be archived under more than one channel, so its source
-- publish date lives in `channel_videos`, keyed by (channel_id, video_id),
-- not on the canonical `videos` row. Several read paths (continue-watching
-- and watch-again feeds, the liked list, the hidden list) need to surface
-- that date, and each had grown its own copy of the same correlated
-- subquery — an identical "prefer the row matching the video's channel,
-- else the most-recently-seen dated row" rule duplicated 5+ times and at
-- risk of drifting apart.
--
-- This view expresses that rule once. Callers `LEFT JOIN
-- video_published_at vpa ON vpa.video_id = <their videos alias>.video_id`
-- and select `vpa.published_at` instead of inlining the subquery.
--
-- Resolution, per video (keyed off `videos.channel_id`):
--   1. the `channel_videos` row whose `channel_id` matches the video's
--      canonical channel and carries a non-null `published_at`, else
--   2. the most-recently-seen (`last_seen_at DESC`) dated row for that
--      video across any channel.
-- Returns NULL when no `channel_videos` row carries a date.
--
-- `is_deleted` rows are intentionally NOT filtered: a tombstoned archive
-- row still records when the video was published, and the list/feed
-- queries that consume this view already gate visibility elsewhere. This
-- matches the behaviour of the inline subqueries it replaces.
--
-- Note: the player metadata path (`routes::videos::lookup_published_at`)
-- deliberately does NOT use this view — it resolves a date for videos that
-- may not yet have a `videos` row and prefers an extraction-time channel id
-- rather than `videos.channel_id`, so it keeps its own parameterised query.

CREATE VIEW video_published_at AS
SELECT v.video_id AS video_id,
       COALESCE(
           (SELECT cv.published_at
              FROM channel_videos cv
             WHERE cv.video_id = v.video_id
               AND cv.channel_id = v.channel_id
               AND cv.published_at IS NOT NULL
             LIMIT 1),
           (SELECT cv.published_at
              FROM channel_videos cv
             WHERE cv.video_id = v.video_id
               AND cv.published_at IS NOT NULL
             ORDER BY cv.last_seen_at DESC
             LIMIT 1)
       ) AS published_at
  FROM videos v;
