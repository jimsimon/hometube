-- Remove the per-child day/hour watch-limit feature.
--
-- Drops the `usage_limits` table and rebuilds `parent_notifications` so its
-- CHECK constraint no longer lists `time_limit_approaching` /
-- `time_limit_reached`. Existing rows of those types are deleted first so the
-- rebuild does not violate the new CHECK.
--
-- `usage_log` is retained: heartbeat/activity tracking still uses it.

COMMIT;
PRAGMA foreign_keys = OFF;
BEGIN;

-- 1. Drop usage_limits outright.
DROP TABLE usage_limits;

-- 2. Rebuild parent_notifications with the trimmed CHECK list. The
--    copy step filters out any pre-existing time_limit_* rows defensively
--    rather than relying on a separate DELETE, so the migration is
--    idempotent even if the table somehow drifts.
CREATE TABLE parent_notifications_new (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    parent_account_id INTEGER NOT NULL REFERENCES accounts(id),
    notification_type TEXT NOT NULL CHECK (notification_type IN (
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
WHERE notification_type NOT IN ('time_limit_approaching', 'time_limit_reached');
DROP TABLE parent_notifications;
ALTER TABLE parent_notifications_new RENAME TO parent_notifications;

COMMIT;

PRAGMA foreign_key_check;

PRAGMA foreign_keys = ON;

BEGIN;
