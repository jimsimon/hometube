-- Audit log of cache evictions (both the manual UI "clear" actions and
-- the scheduled cleanup job's allowlist + LRU passes). Surfaced in the
-- parent System → Cache Management page so admins can answer
-- "why did this video's cache disappear?".
CREATE TABLE cache_evictions (
    id              INTEGER PRIMARY KEY AUTOINCREMENT,
    video_id        TEXT    NOT NULL,
    segment_count   INTEGER NOT NULL DEFAULT 0,
    bytes_freed     INTEGER NOT NULL DEFAULT 0,
    -- Why the eviction happened. One of:
    --   'manual'          — parent clicked "Clear video cache" in the UI
    --   'clear_all'       — parent clicked "Clear entire cache"
    --   'not_allowlisted' — cleanup job: video no longer on any allowlist
    --   'lru_size_limit'  — cleanup job: cache over configured max size
    reason          TEXT    NOT NULL CHECK (
        reason IN ('manual', 'clear_all', 'not_allowlisted', 'lru_size_limit')
    ),
    evicted_at      INTEGER NOT NULL DEFAULT (unixepoch())
);

CREATE INDEX idx_cache_evictions_evicted_at
    ON cache_evictions (evicted_at DESC);
CREATE INDEX idx_cache_evictions_video_id
    ON cache_evictions (video_id);
