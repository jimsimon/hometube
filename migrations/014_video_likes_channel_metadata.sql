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
