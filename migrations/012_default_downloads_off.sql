-- Flip the default of `downloads_enabled` from 1 → 0. New child
-- accounts should opt in to offline downloads explicitly, matching
-- the parent-controlled posture we already use for chromecast
-- (migration 011) and aligning with the principle that destructive
-- or storage-using features start off.
--
-- SQLite can't ALTER COLUMN to change DEFAULT, so we rebuild the
-- table. Existing rows are preserved verbatim — flipping the default
-- intentionally does NOT retroactively disable downloads for kids
-- whose parents previously enabled them (an unexpected app-update
-- regression would be worse than a slightly-permissive existing
-- setting).
CREATE TABLE child_settings_new (
    child_account_id INTEGER PRIMARY KEY REFERENCES accounts(id),
    downloads_enabled INTEGER NOT NULL DEFAULT 0,
    max_quality TEXT DEFAULT NULL,
    playback_speed_locked INTEGER NOT NULL DEFAULT 0,
    autoplay_enabled INTEGER NOT NULL DEFAULT 1,
    autoplay_max_consecutive INTEGER DEFAULT NULL,
    chromecast_enabled INTEGER NOT NULL DEFAULT 0,
    created_at INTEGER NOT NULL DEFAULT (unixepoch()),
    updated_at INTEGER NOT NULL DEFAULT (unixepoch())
);

INSERT INTO child_settings_new
    (child_account_id, downloads_enabled, max_quality, playback_speed_locked,
     autoplay_enabled, autoplay_max_consecutive, chromecast_enabled,
     created_at, updated_at)
SELECT child_account_id, downloads_enabled, max_quality, playback_speed_locked,
       autoplay_enabled, autoplay_max_consecutive, chromecast_enabled,
       created_at, updated_at
FROM child_settings;

DROP TABLE child_settings;
ALTER TABLE child_settings_new RENAME TO child_settings;
