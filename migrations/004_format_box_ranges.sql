-- Cache table for mp4 top-level box byte ranges.
--
-- Used by the synthesized DASH manifest to emit `<SegmentBase indexRange>`
-- and `<Initialization range>` elements. Each row records the byte
-- offsets of the `moov` and `sidx` boxes in the upstream mp4 file for a
-- specific (video_id, format_id) pair. These offsets are file-stable
-- (they describe the structure of the underlying media file, not the
-- expiring `videoplayback?expire=...` URL), so once we've probed a
-- format we never need to probe it again.
--
-- Probing is performed lazily at manifest-load time by issuing
-- `Range: bytes=0-65535` against the format URL and parsing the
-- top-level mp4 box list. Failed probes are simply not recorded; the
-- synthesizer falls back to a plain `<BaseURL>` representation for
-- those formats.

CREATE TABLE format_box_ranges (
    video_id TEXT NOT NULL,
    format_id TEXT NOT NULL,
    init_start INTEGER NOT NULL,
    init_end INTEGER NOT NULL,
    index_start INTEGER NOT NULL,
    index_end INTEGER NOT NULL,
    cached_at INTEGER NOT NULL DEFAULT (unixepoch()),
    PRIMARY KEY (video_id, format_id)
);
