-- Remove YouTube sync artefacts.
--
-- The bidirectional YouTube sync feature has been removed. Children are
-- now local-only accounts and all YouTube data is fetched on-demand via
-- the parent's API key. This migration drops:
--
--   • The `sync_state` table (sync progress tracking)
--   • `youtube_linked` from `accounts`
--   • `sync_status`, `source`, `youtube_subscription_id` from `child_subscriptions`
--   • `sync_status`, `source` from `child_playlists`
--   • `sync_status`, `source` from `video_likes`
--   • `sync_error` from `parent_notifications` CHECK constraint
--
-- SQLite requires table rebuilds to remove columns; we break out of
-- sqlx's migration transaction to disable FK enforcement during rebuilds.

COMMIT;
PRAGMA foreign_keys = OFF;
BEGIN;

-- 1. Drop sync_state entirely.
DROP TABLE IF EXISTS sync_state;

-- 2. Rebuild accounts — drop youtube_linked.
CREATE TABLE accounts_new (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    google_id TEXT UNIQUE,
    email TEXT NOT NULL DEFAULT '',
    display_name TEXT NOT NULL,
    avatar_url TEXT,
    account_type TEXT NOT NULL CHECK (account_type IN ('parent', 'child')),
    pin_hash TEXT,
    access_token TEXT NOT NULL DEFAULT '',
    refresh_token TEXT NOT NULL DEFAULT '',
    token_expires_at INTEGER NOT NULL DEFAULT 0,
    created_at INTEGER NOT NULL DEFAULT (unixepoch()),
    updated_at INTEGER NOT NULL DEFAULT (unixepoch())
);
INSERT INTO accounts_new (
    id, google_id, email, display_name, avatar_url, account_type,
    pin_hash, access_token, refresh_token, token_expires_at,
    created_at, updated_at
)
SELECT
    id, google_id, email, display_name, avatar_url, account_type,
    pin_hash, access_token, refresh_token, token_expires_at,
    created_at, updated_at
FROM accounts;
DROP TABLE accounts;
ALTER TABLE accounts_new RENAME TO accounts;

-- 3. Rebuild child_subscriptions — drop sync_status, source, youtube_subscription_id.
CREATE TABLE child_subscriptions_new (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    child_account_id INTEGER NOT NULL REFERENCES accounts(id),
    channel_id TEXT NOT NULL,
    channel_title TEXT NOT NULL,
    channel_thumbnail_url TEXT,
    is_deleted INTEGER NOT NULL DEFAULT 0,
    subscribed_at INTEGER NOT NULL DEFAULT (unixepoch()),
    updated_at INTEGER NOT NULL DEFAULT (unixepoch()),
    UNIQUE(child_account_id, channel_id)
);
INSERT INTO child_subscriptions_new (
    id, child_account_id, channel_id, channel_title, channel_thumbnail_url,
    is_deleted, subscribed_at, updated_at
)
SELECT
    id, child_account_id, channel_id, channel_title, channel_thumbnail_url,
    is_deleted, subscribed_at, updated_at
FROM child_subscriptions;
DROP TABLE child_subscriptions;
ALTER TABLE child_subscriptions_new RENAME TO child_subscriptions;

-- 4. Rebuild child_playlists — drop sync_status, source.
CREATE TABLE child_playlists_new (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    child_account_id INTEGER NOT NULL REFERENCES accounts(id),
    youtube_playlist_id TEXT,
    title TEXT NOT NULL,
    description TEXT,
    is_own INTEGER NOT NULL DEFAULT 1,
    is_deleted INTEGER NOT NULL DEFAULT 0,
    created_at INTEGER NOT NULL DEFAULT (unixepoch()),
    updated_at INTEGER NOT NULL DEFAULT (unixepoch())
);
INSERT INTO child_playlists_new (
    id, child_account_id, youtube_playlist_id, title, description,
    is_own, is_deleted, created_at, updated_at
)
SELECT
    id, child_account_id, youtube_playlist_id, title, description,
    is_own, is_deleted, created_at, updated_at
FROM child_playlists;
DROP TABLE child_playlists;
ALTER TABLE child_playlists_new RENAME TO child_playlists;

-- 5. Rebuild video_likes — drop sync_status, source.
CREATE TABLE video_likes_new (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    child_account_id INTEGER NOT NULL REFERENCES accounts(id),
    video_id TEXT NOT NULL,
    video_title TEXT,
    video_thumbnail_url TEXT,
    is_deleted INTEGER NOT NULL DEFAULT 0,
    liked_at INTEGER NOT NULL DEFAULT (unixepoch()),
    updated_at INTEGER NOT NULL DEFAULT (unixepoch()),
    UNIQUE(child_account_id, video_id)
);
INSERT INTO video_likes_new (
    id, child_account_id, video_id, video_title, video_thumbnail_url,
    is_deleted, liked_at, updated_at
)
SELECT
    id, child_account_id, video_id, video_title, video_thumbnail_url,
    is_deleted, liked_at, updated_at
FROM video_likes;
DROP TABLE video_likes;
ALTER TABLE video_likes_new RENAME TO video_likes;

-- 6. Rebuild parent_notifications — remove 'sync_error' from CHECK,
--    remove 'token_expired' (no longer relevant without sync).
CREATE TABLE parent_notifications_new (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    parent_account_id INTEGER NOT NULL REFERENCES accounts(id),
    notification_type TEXT NOT NULL CHECK (notification_type IN (
        'time_limit_approaching', 'time_limit_reached',
        'ytdlp_failure',
        'new_search_term', 'system_update'
    )),
    title TEXT NOT NULL,
    message TEXT NOT NULL,
    metadata TEXT,
    is_read INTEGER NOT NULL DEFAULT 0,
    created_at INTEGER NOT NULL DEFAULT (unixepoch())
);
INSERT INTO parent_notifications_new (
    id, parent_account_id, notification_type, title, message,
    metadata, is_read, created_at
)
SELECT
    id, parent_account_id, notification_type, title, message,
    metadata, is_read, created_at
FROM parent_notifications
WHERE notification_type != 'sync_error' AND notification_type != 'token_expired';
DROP TABLE parent_notifications;
ALTER TABLE parent_notifications_new RENAME TO parent_notifications;

COMMIT;

-- Verify no FK violations.
PRAGMA foreign_key_check;

PRAGMA foreign_keys = ON;

-- Re-enter a transaction for sqlx bookkeeping.
BEGIN;
