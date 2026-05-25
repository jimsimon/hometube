-- On-disk thumbnail cache.
--
-- `/api/proxy/thumbnail/:videoId` previously reverse-proxied every
-- request to `i.ytimg.com/vi/<id>/...` with no caching layer (each
-- child page load re-fetched every thumbnail). This table tracks the
-- on-disk thumbnail blobs so the proxy can serve cache hits without
-- hitting YouTube, modelled on the existing `segment_cache` pattern
-- (see migration 001 + `src/services/segment_store.rs`).
--
-- One row per cached video_id. Thumbnails are derived URLs
-- (`hqdefault.jpg` with `mqdefault.jpg` fallback) — there's only one
-- per video — so the PRIMARY KEY is just `video_id`.

CREATE TABLE thumbnail_cache (
    video_id          TEXT    PRIMARY KEY,
    file_path         TEXT    NOT NULL,
    file_size_bytes   INTEGER NOT NULL CHECK (file_size_bytes >= 0),
    cached_at         INTEGER NOT NULL DEFAULT (unixepoch()),
    last_accessed_at  INTEGER NOT NULL DEFAULT (unixepoch())
);

CREATE INDEX idx_thumbnail_cache_lru ON thumbnail_cache(last_accessed_at ASC);
