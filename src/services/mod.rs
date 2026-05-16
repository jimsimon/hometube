//! Business-logic services.
//!
//! - [`oauth`]: Google OAuth2 client construction, token exchange + refresh,
//!   and userinfo fetch
//! - [`setup`]: small typed helpers for reading/writing the `app_config`
//!   table (used by the wizard and the setup-redirect middleware)
//! - [`youtube`]: YouTube Data API v3 read client (search, channels,
//!   playlists, videos)
//! - [`ytdlp`]: yt-dlp subprocess wrapper for video extraction
//! - [`video_cache`]: two-layer (memory + DB) yt-dlp metadata cache
//! - [`dash`]: DASH manifest rewriter + HMAC signing helpers for the
//!   segment proxy
//! - [`access`]: child content-access decisions (allowlist + blocklist)
//! - [`cron`]: in-process cron scheduler + default-job seeding +
//!   yt-dlp / cache-cleanup handlers

pub mod access;
pub mod cron;
pub mod dash;
pub mod hls;
pub mod mp4;
pub mod notifications;
pub mod oauth;
pub mod setup;
pub mod video_cache;
pub mod youtube;
pub mod ytdlp;
