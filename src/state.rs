//! Shared application state passed to every Axum handler.

use sqlx::SqlitePool;

use crate::config::Config;

/// Cheap-to-clone bundle of dependencies shared across handlers.
#[derive(Clone)]
pub struct AppState {
    pub config: Config,
    pub db: SqlitePool,
}

impl AppState {
    pub fn new(config: Config, db: SqlitePool) -> Self {
        Self { config, db }
    }
}
