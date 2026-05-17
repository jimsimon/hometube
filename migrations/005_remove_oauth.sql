-- Remove Google OAuth artefacts.
--
-- Google OAuth was previously required for YouTube Data API access, but
-- that dependency has been removed. Authentication is now handled
-- entirely via the existing PIN system; accounts are created locally
-- with a display name and optional PIN.
--
-- This migration drops:
--   • `google_id`, `email`, `access_token`, `refresh_token`,
--     `token_expires_at` from `accounts`
--   • Google credential entries from `app_config`
--
-- SQLite requires a table rebuild to remove columns; we break out of
-- sqlx's migration transaction to disable FK enforcement during the
-- rebuild.

COMMIT;
PRAGMA foreign_keys = OFF;
BEGIN;

-- 1. Rebuild accounts — drop OAuth columns.
CREATE TABLE accounts_new (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    display_name TEXT NOT NULL,
    avatar_url TEXT,
    account_type TEXT NOT NULL CHECK (account_type IN ('parent', 'child')),
    pin_hash TEXT,
    created_at INTEGER NOT NULL DEFAULT (unixepoch()),
    updated_at INTEGER NOT NULL DEFAULT (unixepoch())
);
INSERT INTO accounts_new (
    id, display_name, avatar_url, account_type, pin_hash,
    created_at, updated_at
)
SELECT
    id, display_name, avatar_url, account_type, pin_hash,
    created_at, updated_at
FROM accounts;
DROP TABLE accounts;
ALTER TABLE accounts_new RENAME TO accounts;

-- 2. Remove stale Google credential entries from app_config.
DELETE FROM app_config WHERE key IN (
    'google_client_id',
    'google_client_secret',
    'google_redirect_uri'
);

COMMIT;

-- Verify no FK violations were introduced.
PRAGMA foreign_key_check;

PRAGMA foreign_keys = ON;

-- Re-enter a transaction so sqlx can record the migration.
BEGIN;
