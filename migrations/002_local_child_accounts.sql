-- Allow child accounts to exist without a linked Google/YouTube account.
--
-- Local-only children have google_id = NULL, empty tokens, and
-- youtube_linked = 0. SQLite's UNIQUE constraint treats each NULL as
-- distinct, so multiple local-only accounts are allowed.
--
-- The original schema has google_id as NOT NULL, so we must rebuild the
-- table to make it nullable. SQLite requires PRAGMA foreign_keys = OFF
-- for safe table rebuilds, but that PRAGMA is a no-op inside a
-- transaction. We break out of sqlx's migration transaction, do the
-- rebuild, then re-enter a transaction for sqlx's bookkeeping.

-- Break out of sqlx's implicit migration transaction.
COMMIT;

PRAGMA foreign_keys = OFF;

BEGIN;

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
    youtube_linked INTEGER NOT NULL DEFAULT 0,
    created_at INTEGER NOT NULL DEFAULT (unixepoch()),
    updated_at INTEGER NOT NULL DEFAULT (unixepoch())
);

-- Copy existing data. All existing accounts are Google-linked.
INSERT INTO accounts_new (
    id, google_id, email, display_name, avatar_url, account_type,
    pin_hash, access_token, refresh_token, token_expires_at,
    youtube_linked, created_at, updated_at
)
SELECT
    id, google_id, email, display_name, avatar_url, account_type,
    pin_hash, access_token, refresh_token, token_expires_at,
    1, created_at, updated_at
FROM accounts;

DROP TABLE accounts;
ALTER TABLE accounts_new RENAME TO accounts;

COMMIT;

-- Verify no FK violations were introduced.
PRAGMA foreign_key_check;

PRAGMA foreign_keys = ON;

-- Re-enter a transaction so sqlx can record the migration.
BEGIN;
