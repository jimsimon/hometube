-- Extend parent_notifications CHECK to include 'channel_backfill_error'.
--
-- Migration 016 (`remove_usage_limits.sql`) is the canonical recent
-- precedent for this rebuild pattern: the SQLite CHECK constraint can't
-- be altered in place, so we create a new table, copy the rows, drop
-- the old, rename, and recreate any indexes.

COMMIT;
PRAGMA foreign_keys = OFF;
BEGIN;

CREATE TABLE parent_notifications_new (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    parent_account_id INTEGER NOT NULL REFERENCES accounts(id),
    notification_type TEXT NOT NULL CHECK (notification_type IN (
        'ytdlp_failure',
        'new_search_term',
        'system_update',
        'channel_backfill_error'
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
FROM parent_notifications;
DROP TABLE parent_notifications;
ALTER TABLE parent_notifications_new RENAME TO parent_notifications;

COMMIT;

PRAGMA foreign_key_check;

PRAGMA foreign_keys = ON;

BEGIN;
