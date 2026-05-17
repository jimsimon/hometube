//! Setup-state helpers.
//!
//! HomeTube stores runtime-configurable settings — the cookie signing key
//! and the `setup_complete` flag — in the `app_config` table. This module
//! provides small typed helpers for reading/writing those entries and a
//! single [`is_setup_complete`] check used by the setup-redirect
//! middleware.

use sqlx::SqlitePool;
use tracing::debug;

use crate::error::AppResult;

/// Key in `app_config` that flips to `"true"` once the setup wizard
/// finishes.
pub const KEY_SETUP_COMPLETE: &str = "setup_complete";
pub const KEY_COOKIE_SECRET: &str = "cookie_secret";
pub const KEY_YTDLP_COOKIES: &str = "ytdlp_cookies";

/// Look up a single value from `app_config`. Returns [`None`] if the key
/// is not set.
pub async fn get_config_value(pool: &SqlitePool, key: &str) -> AppResult<Option<String>> {
    let row: Option<(String,)> = sqlx::query_as("SELECT value FROM app_config WHERE key = ?")
        .bind(key)
        .fetch_optional(pool)
        .await?;
    Ok(row.map(|(v,)| v))
}

/// Insert or update a single `app_config` entry.
pub async fn set_config_value(pool: &SqlitePool, key: &str, value: &str) -> AppResult<()> {
    sqlx::query(
        "INSERT INTO app_config (key, value) VALUES (?, ?) \
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
    )
    .bind(key)
    .bind(value)
    .execute(pool)
    .await?;
    debug!(%key, "app_config value updated");
    Ok(())
}

/// True once the setup wizard has marked the install complete.
pub async fn is_setup_complete(pool: &SqlitePool) -> AppResult<bool> {
    Ok(get_config_value(pool, KEY_SETUP_COMPLETE)
        .await?
        .map(|v| v == "true")
        .unwrap_or(false))
}

/// True if at least one parent account exists.
pub async fn has_first_parent(pool: &SqlitePool) -> AppResult<bool> {
    let row: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM accounts WHERE account_type = 'parent'")
        .fetch_one(pool)
        .await?;
    Ok(row.0 > 0)
}
