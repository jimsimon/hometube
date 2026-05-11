//! Shared application state passed to every Axum handler.
//!
//! Bundles the static [`Config`] (process-level defaults), the SQLite
//! connection pool, and the signing [`Key`] used by `tower-cookies` for
//! signed session cookies. The cookie key is loaded (or generated) from the
//! `app_config` table on startup so that signed cookies remain valid across
//! restarts.

use sqlx::SqlitePool;
use tower_cookies::Key;

use crate::config::Config;

/// Cheap-to-clone bundle of dependencies shared across handlers.
///
/// `Key` is cheap to clone (it wraps an `Arc`-like internal buffer) and
/// `SqlitePool` is itself a clonable handle, so cloning [`AppState`] is
/// effectively zero-cost.
#[derive(Clone)]
pub struct AppState {
    pub config: Config,
    pub db: SqlitePool,
    /// Master key used to sign session cookies.
    pub cookie_key: Key,
}

impl AppState {
    pub fn new(config: Config, db: SqlitePool, cookie_key: Key) -> Self {
        Self {
            config,
            db,
            cookie_key,
        }
    }
}
