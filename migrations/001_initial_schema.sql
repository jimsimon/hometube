-- Initial schema for HomeTube.
--
-- See the implementation plan for the full ERD. Tables are introduced
-- together so that foreign-key references resolve cleanly within a single
-- migration.

-- =========================================================================
-- Accounts & sessions
-- =========================================================================

CREATE TABLE accounts (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    google_id TEXT UNIQUE NOT NULL,
    email TEXT NOT NULL,
    display_name TEXT NOT NULL,
    avatar_url TEXT,
    account_type TEXT NOT NULL CHECK (account_type IN ('parent', 'child')),
    pin_hash TEXT,
    access_token TEXT NOT NULL,
    refresh_token TEXT NOT NULL,
    token_expires_at INTEGER NOT NULL,
    created_at INTEGER NOT NULL DEFAULT (unixepoch()),
    updated_at INTEGER NOT NULL DEFAULT (unixepoch())
);

CREATE TABLE sessions (
    id TEXT PRIMARY KEY,
    account_id INTEGER NOT NULL REFERENCES accounts(id),
    expires_at INTEGER NOT NULL,
    created_at INTEGER NOT NULL DEFAULT (unixepoch())
);

-- =========================================================================
-- Allowlists & blocklists
-- =========================================================================

CREATE TABLE allowlisted_channels (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    child_account_id INTEGER NOT NULL REFERENCES accounts(id),
    channel_id TEXT NOT NULL,
    channel_title TEXT NOT NULL,
    channel_thumbnail_url TEXT,
    added_by INTEGER NOT NULL REFERENCES accounts(id),
    created_at INTEGER NOT NULL DEFAULT (unixepoch()),
    UNIQUE(child_account_id, channel_id)
);

CREATE TABLE allowlisted_playlists (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    child_account_id INTEGER NOT NULL REFERENCES accounts(id),
    playlist_id TEXT NOT NULL,
    playlist_title TEXT NOT NULL,
    playlist_thumbnail_url TEXT,
    added_by INTEGER NOT NULL REFERENCES accounts(id),
    created_at INTEGER NOT NULL DEFAULT (unixepoch()),
    UNIQUE(child_account_id, playlist_id)
);

CREATE TABLE allowlisted_videos (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    child_account_id INTEGER NOT NULL REFERENCES accounts(id),
    video_id TEXT NOT NULL,
    video_title TEXT NOT NULL,
    video_thumbnail_url TEXT,
    channel_title TEXT,
    added_by INTEGER NOT NULL REFERENCES accounts(id),
    created_at INTEGER NOT NULL DEFAULT (unixepoch()),
    UNIQUE(child_account_id, video_id)
);

CREATE TABLE blocked_videos (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    child_account_id INTEGER NOT NULL REFERENCES accounts(id),
    video_id TEXT NOT NULL,
    video_title TEXT,
    blocked_by INTEGER NOT NULL REFERENCES accounts(id),
    reason TEXT,
    created_at INTEGER NOT NULL DEFAULT (unixepoch()),
    UNIQUE(child_account_id, video_id)
);

-- =========================================================================
-- Per-child settings & usage limits
-- =========================================================================

CREATE TABLE child_settings (
    child_account_id INTEGER PRIMARY KEY REFERENCES accounts(id),
    downloads_enabled INTEGER NOT NULL DEFAULT 1,
    max_quality TEXT DEFAULT NULL,
    playback_speed_locked INTEGER NOT NULL DEFAULT 0,
    autoplay_enabled INTEGER NOT NULL DEFAULT 1,
    autoplay_max_consecutive INTEGER DEFAULT NULL,
    created_at INTEGER NOT NULL DEFAULT (unixepoch()),
    updated_at INTEGER NOT NULL DEFAULT (unixepoch())
);

CREATE TABLE usage_limits (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    child_account_id INTEGER NOT NULL REFERENCES accounts(id),
    day_of_week INTEGER NOT NULL CHECK (day_of_week BETWEEN 0 AND 6),
    max_hours REAL NOT NULL DEFAULT 2.0,
    allowed_start_time TEXT NOT NULL DEFAULT '08:00',
    allowed_end_time TEXT NOT NULL DEFAULT '20:00',
    created_at INTEGER NOT NULL DEFAULT (unixepoch()),
    updated_at INTEGER NOT NULL DEFAULT (unixepoch()),
    UNIQUE(child_account_id, day_of_week)
);

CREATE TABLE usage_log (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    child_account_id INTEGER NOT NULL REFERENCES accounts(id),
    video_id TEXT NOT NULL,
    started_at INTEGER NOT NULL,
    ended_at INTEGER,
    duration_seconds INTEGER
);

-- =========================================================================
-- Child playlists, subscriptions, watch history, likes (YouTube-synced)
-- =========================================================================

CREATE TABLE child_playlists (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    child_account_id INTEGER NOT NULL REFERENCES accounts(id),
    youtube_playlist_id TEXT,
    title TEXT NOT NULL,
    description TEXT,
    is_own INTEGER NOT NULL DEFAULT 1,
    source TEXT NOT NULL DEFAULT 'app' CHECK (source IN ('app', 'youtube')),
    sync_status TEXT NOT NULL DEFAULT 'synced'
        CHECK (sync_status IN ('synced', 'pending_create', 'pending_update', 'pending_delete', 'error')),
    is_deleted INTEGER NOT NULL DEFAULT 0,
    created_at INTEGER NOT NULL DEFAULT (unixepoch()),
    updated_at INTEGER NOT NULL DEFAULT (unixepoch())
);

CREATE TABLE child_playlist_videos (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    playlist_id INTEGER NOT NULL REFERENCES child_playlists(id) ON DELETE CASCADE,
    video_id TEXT NOT NULL,
    video_title TEXT NOT NULL,
    video_thumbnail_url TEXT,
    channel_title TEXT,
    position INTEGER NOT NULL,
    added_at INTEGER NOT NULL DEFAULT (unixepoch()),
    UNIQUE(playlist_id, video_id)
);

CREATE TABLE child_subscriptions (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    child_account_id INTEGER NOT NULL REFERENCES accounts(id),
    channel_id TEXT NOT NULL,
    channel_title TEXT NOT NULL,
    channel_thumbnail_url TEXT,
    youtube_subscription_id TEXT,
    source TEXT NOT NULL DEFAULT 'app' CHECK (source IN ('app', 'youtube')),
    sync_status TEXT NOT NULL DEFAULT 'synced'
        CHECK (sync_status IN ('synced', 'pending_push', 'pending_delete', 'error')),
    is_deleted INTEGER NOT NULL DEFAULT 0,
    subscribed_at INTEGER NOT NULL DEFAULT (unixepoch()),
    updated_at INTEGER NOT NULL DEFAULT (unixepoch()),
    UNIQUE(child_account_id, channel_id)
);

CREATE TABLE watch_history (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    child_account_id INTEGER NOT NULL REFERENCES accounts(id),
    video_id TEXT NOT NULL,
    video_title TEXT NOT NULL,
    video_thumbnail_url TEXT,
    channel_title TEXT,
    duration_seconds INTEGER,
    progress_seconds INTEGER NOT NULL DEFAULT 0,
    last_watched_at INTEGER NOT NULL DEFAULT (unixepoch()),
    UNIQUE(child_account_id, video_id)
);

CREATE TABLE video_likes (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    child_account_id INTEGER NOT NULL REFERENCES accounts(id),
    video_id TEXT NOT NULL,
    video_title TEXT,
    video_thumbnail_url TEXT,
    source TEXT NOT NULL DEFAULT 'app' CHECK (source IN ('app', 'youtube')),
    sync_status TEXT NOT NULL DEFAULT 'synced'
        CHECK (sync_status IN ('synced', 'pending_push', 'pending_delete', 'error')),
    is_deleted INTEGER NOT NULL DEFAULT 0,
    liked_at INTEGER NOT NULL DEFAULT (unixepoch()),
    updated_at INTEGER NOT NULL DEFAULT (unixepoch()),
    UNIQUE(child_account_id, video_id)
);

CREATE TABLE sync_state (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    account_id INTEGER NOT NULL REFERENCES accounts(id),
    data_type TEXT NOT NULL CHECK (data_type IN ('subscriptions', 'likes', 'playlists')),
    last_synced_at INTEGER,
    last_page_token TEXT,
    etag TEXT,
    UNIQUE(account_id, data_type)
);

-- =========================================================================
-- App config & cron jobs
-- =========================================================================

CREATE TABLE app_config (
    key TEXT PRIMARY KEY,
    value TEXT NOT NULL
);

CREATE TABLE cron_jobs (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    name TEXT UNIQUE NOT NULL,
    description TEXT,
    job_type TEXT NOT NULL,
    schedule TEXT NOT NULL,
    schedule_preset TEXT,
    allowed_presets TEXT NOT NULL,
    enabled INTEGER NOT NULL DEFAULT 1,
    last_run_at INTEGER,
    last_run_status TEXT CHECK (last_run_status IN ('success', 'failure', NULL)),
    last_run_message TEXT,
    next_run_at INTEGER,
    created_at INTEGER NOT NULL DEFAULT (unixepoch())
);

CREATE TABLE cron_job_runs (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    job_id INTEGER NOT NULL REFERENCES cron_jobs(id),
    started_at INTEGER NOT NULL,
    finished_at INTEGER,
    status TEXT NOT NULL CHECK (status IN ('running', 'success', 'failure')),
    message TEXT,
    output TEXT
);

CREATE TABLE ytdlp_info (
    id INTEGER PRIMARY KEY CHECK (id = 1),
    current_version TEXT,
    last_checked_at INTEGER,
    last_updated_at INTEGER,
    binary_path TEXT NOT NULL
);

-- =========================================================================
-- Caching tables
-- =========================================================================

CREATE TABLE segment_cache (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    video_id TEXT NOT NULL,
    format_id TEXT NOT NULL,
    segment_number INTEGER NOT NULL,
    file_path TEXT NOT NULL,
    file_size_bytes INTEGER NOT NULL,
    cached_at INTEGER NOT NULL DEFAULT (unixepoch()),
    last_accessed_at INTEGER NOT NULL DEFAULT (unixepoch()),
    UNIQUE(video_id, format_id, segment_number)
);

CREATE INDEX idx_segment_cache_lru ON segment_cache(last_accessed_at ASC);

CREATE TABLE video_metadata_cache (
    video_id TEXT PRIMARY KEY,
    metadata_json TEXT NOT NULL,
    dash_manifest TEXT,
    cached_at INTEGER NOT NULL DEFAULT (unixepoch()),
    expires_at INTEGER NOT NULL
);

-- =========================================================================
-- Search log, bookmarks, family playlists, notifications, sleep timers,
-- offline downloads
-- =========================================================================

CREATE TABLE search_log (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    child_account_id INTEGER NOT NULL REFERENCES accounts(id),
    query TEXT NOT NULL,
    result_count INTEGER NOT NULL DEFAULT 0,
    searched_at INTEGER NOT NULL DEFAULT (unixepoch())
);

CREATE TABLE video_bookmarks (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    child_account_id INTEGER NOT NULL REFERENCES accounts(id),
    video_id TEXT NOT NULL,
    video_title TEXT,
    timestamp_seconds INTEGER NOT NULL,
    label TEXT,
    created_at INTEGER NOT NULL DEFAULT (unixepoch()),
    UNIQUE(child_account_id, video_id, timestamp_seconds)
);

CREATE TABLE family_playlists (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    created_by INTEGER NOT NULL REFERENCES accounts(id),
    title TEXT NOT NULL,
    description TEXT,
    created_at INTEGER NOT NULL DEFAULT (unixepoch()),
    updated_at INTEGER NOT NULL DEFAULT (unixepoch())
);

CREATE TABLE family_playlist_members (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    playlist_id INTEGER NOT NULL REFERENCES family_playlists(id) ON DELETE CASCADE,
    child_account_id INTEGER NOT NULL REFERENCES accounts(id),
    UNIQUE(playlist_id, child_account_id)
);

CREATE TABLE family_playlist_videos (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    playlist_id INTEGER NOT NULL REFERENCES family_playlists(id) ON DELETE CASCADE,
    video_id TEXT NOT NULL,
    video_title TEXT NOT NULL,
    video_thumbnail_url TEXT,
    channel_title TEXT,
    position INTEGER NOT NULL,
    added_at INTEGER NOT NULL DEFAULT (unixepoch()),
    UNIQUE(playlist_id, video_id)
);

CREATE TABLE parent_notifications (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    parent_account_id INTEGER NOT NULL REFERENCES accounts(id),
    notification_type TEXT NOT NULL CHECK (notification_type IN (
        'time_limit_approaching', 'time_limit_reached',
        'ytdlp_failure', 'sync_error', 'token_expired',
        'new_search_term', 'system_update'
    )),
    title TEXT NOT NULL,
    message TEXT NOT NULL,
    metadata TEXT,
    is_read INTEGER NOT NULL DEFAULT 0,
    created_at INTEGER NOT NULL DEFAULT (unixepoch())
);

CREATE TABLE sleep_timers (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    child_account_id INTEGER NOT NULL REFERENCES accounts(id),
    timer_type TEXT NOT NULL CHECK (timer_type IN ('after_video', 'minutes')),
    minutes_remaining INTEGER,
    videos_remaining INTEGER DEFAULT 1,
    started_at INTEGER NOT NULL DEFAULT (unixepoch()),
    expires_at INTEGER,
    is_active INTEGER NOT NULL DEFAULT 1,
    UNIQUE(child_account_id, is_active)
);

CREATE TABLE offline_downloads (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    child_account_id INTEGER NOT NULL REFERENCES accounts(id),
    video_id TEXT NOT NULL,
    video_title TEXT NOT NULL,
    video_thumbnail_url TEXT,
    channel_title TEXT,
    quality_label TEXT NOT NULL,
    file_size_bytes INTEGER,
    duration_seconds INTEGER,
    status TEXT NOT NULL DEFAULT 'pending'
        CHECK (status IN ('pending', 'downloading', 'complete', 'failed', 'deleted')),
    downloaded_at INTEGER,
    UNIQUE(child_account_id, video_id, quality_label)
);
