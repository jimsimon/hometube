-- Per-child "Hidden Videos" feature.
--
-- A child can mark a video as hidden; it then disappears from every
-- listing surface for THAT child only (parent and sibling views are
-- unaffected). Recovery is via the /child/hidden page.
--
-- This is separate from `blocked_videos` (which is parent-managed
-- moderation) to keep the two concepts cleanly distinguished.
CREATE TABLE hidden_videos (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    child_account_id INTEGER NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    video_id TEXT NOT NULL,
    video_title TEXT,
    channel_id TEXT,
    channel_title TEXT,
    video_thumbnail_url TEXT,
    duration_seconds INTEGER,
    hidden_at INTEGER NOT NULL DEFAULT (unixepoch()),
    UNIQUE(child_account_id, video_id)
);

CREATE INDEX idx_hidden_videos_child
    ON hidden_videos(child_account_id, hidden_at DESC);
