//! Shared application state passed to every Axum handler.
//!
//! Bundles the static [`Config`] (process-level defaults), the SQLite
//! connection pool, and the signing [`Key`] used by `tower-cookies` for
//! signed session cookies. The cookie key is loaded (or generated) from the
//! `app_config` table on startup so that signed cookies remain valid across
//! restarts.
//!
//! Phase 12 adds an optional [`crate::services::cron::Scheduler`] handle so
//! the parent-only cron API can trigger jobs immediately. The handle is
//! held inside an `Option` so unit tests can construct an `AppState`
//! without spinning up the full scheduler.

use reqwest::Client;
use sqlx::SqlitePool;
use tower_cookies::Key;

use crate::config::Config;
use crate::services::cron::Scheduler;

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
    /// Optional cron-scheduler handle (Phase 12). `None` in tests.
    pub scheduler: Option<Scheduler>,
    /// Shared HTTP client for upstream requests (connection-pooled).
    pub http_client: Client,
}

impl AppState {
    pub fn new(config: Config, db: SqlitePool, cookie_key: Key) -> Self {
        Self {
            config,
            db,
            cookie_key,
            scheduler: None,
            http_client: Client::new(),
        }
    }

    /// Builder helper — install a [`Scheduler`] handle on this state
    /// before passing it to [`crate::routes::router`].
    pub fn with_scheduler(mut self, scheduler: Scheduler) -> Self {
        self.scheduler = Some(scheduler);
        self
    }
}
