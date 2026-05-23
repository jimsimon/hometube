//! Business-logic services.
//!
//! - [`setup`]: small typed helpers for reading/writing the `app_config`
//!   table (used by the wizard and the setup-redirect middleware)
//! - [`youtube`]: Content discovery client backed by the youtubei.js
//!   sidecar (search, channels, videos)
//! - [`ytdlp`]: yt-dlp subprocess wrapper for video extraction
//! - [`video_cache`]: two-layer (memory + DB) yt-dlp metadata cache
//! - [`dash`]: DASH manifest synthesis + HMAC signing helpers for the
//!   format proxy
//! - [`access`]: child content-access decisions (allowlist + blocklist)
//! - [`cron`]: in-process cron scheduler + default-job seeding +
//!   yt-dlp / cache-cleanup handlers

pub mod access;
pub mod channel_backfill;
pub mod cron;
pub mod dash;
pub mod feed_cache;
pub mod feed_refresher;
pub mod notification_forwarders;
pub mod notifications;
pub mod segment_ranges;
pub mod segment_store;
pub mod setup;
pub mod video_cache;
pub mod youtube;
pub mod youtube_rss;
pub mod ytdlp;
