//! Business-logic services.
//!
//! - [`oauth`]: Google OAuth2 client construction, token exchange + refresh,
//!   and userinfo fetch
//! - [`setup`]: small typed helpers for reading/writing the `app_config`
//!   table (used by the wizard and the setup-redirect middleware)
//!
//! Future phases add `youtube`, `ytdlp`, `video_proxy`, and `cron`.

pub mod oauth;
pub mod setup;
