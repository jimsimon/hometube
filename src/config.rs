//! Runtime configuration.
//!
//! HomeTube is zero-config by default: every value here has a sensible
//! default. Environment variables are only used as overrides for advanced
//! deployments. Application-level settings (OAuth credentials, etc.) are
//! collected by the setup wizard and stored in the `app_config` table — they
//! are not part of [`Config`].

use std::env;

#[derive(Clone, Debug)]
pub struct Config {
    /// HTTP listen host (default: `0.0.0.0`).
    pub host: String,
    /// HTTP listen port (default: `3000`).
    pub port: u16,
    /// Filesystem path to the SQLite database. The directory will be created
    /// on first run.
    pub database_url: String,
    /// Path to the `yt-dlp` binary (default: `yt-dlp`, resolved via `PATH`).
    pub ytdlp_path: String,
    /// Directory where the Vite-built static assets live. Default differs
    /// between dev (`./frontend/dist`) and Docker (`/app/static`).
    pub static_dir: String,
    /// Directory for on-disk segment cache files. Chunks are stored in a
    /// sharded layout: `{cache_dir}/{video_id[0:2]}/{video_id}/{format}_{chunk}.chunk`.
    pub cache_dir: String,
}

impl Config {
    /// Build a [`Config`] from environment variables, falling back to the
    /// documented defaults for any value that is not set.
    pub fn from_env() -> anyhow::Result<Self> {
        let host = env::var("HOST").unwrap_or_else(|_| "0.0.0.0".to_string());
        let port: u16 = env::var("PORT")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(3000);

        let database_path =
            env::var("DATABASE_PATH").unwrap_or_else(|_| "./data/database/app.db".to_string());
        let database_url = format!("sqlite://{database_path}?mode=rwc");

        let ytdlp_path = env::var("YTDLP_PATH").unwrap_or_else(|_| "yt-dlp".to_string());
        let static_dir = env::var("STATIC_DIR").unwrap_or_else(|_| "./frontend/dist".to_string());
        let cache_dir = env::var("CACHE_DIR").unwrap_or_else(|_| "./data/cache".to_string());

        // Ensure the persistent-state directories exist before any
        // Ensure the persistent-state directories exist before any
        // consumer (SQLite, segment store) tries to open files inside
        // them. Each subtree is independent — operators may mount them
        // on entirely separate filesystems. The tools directory is
        // created lazily by its writers (cookies, yt-dlp binary).
        // to open files inside them. Each subtree is independent —
        // created lazily by its writers (cookies, yt-dlp binary).
        if let Some(parent) = std::path::Path::new(&database_path).parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent).ok();
            }
        }
        std::fs::create_dir_all(&cache_dir).ok();

        Ok(Self {
            host,
            port,
            database_url,
            ytdlp_path,
            static_dir,
            cache_dir,
        })
    }
}
