//! Parent-notification dispatcher.
//!
//! All notification inserts in the codebase funnel through [`dispatch`]
//! (or one of the higher-level helpers built on top of it) so we have a
//! single place to control:
//!
//! - validating the notification type against the `parent_notifications`
//!   schema CHECK constraint;
//! - persisting metadata as a JSON string;
//! - applying simple "one per child per day" / "one per key per window"
//!   dedup without polluting handlers with the boilerplate.
//!
//! The `TYPE_*` constants are the canonical strings stored in the
//! `notification_type` column. Always pass one of them to [`dispatch`]
//! (or to the typed wrappers) rather than hand-rolling a literal.

use std::collections::HashMap;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use serde::Serialize;
use sqlx::SqlitePool;
use tokio::sync::Mutex;

use crate::error::AppResult;

/// Recognised values for `parent_notifications.notification_type`.
pub const TYPE_TIME_LIMIT_APPROACHING: &str = "time_limit_approaching";
pub const TYPE_TIME_LIMIT_REACHED: &str = "time_limit_reached";
pub const TYPE_YTDLP_FAILURE: &str = "ytdlp_failure";
pub const TYPE_SYNC_ERROR: &str = "sync_error";
#[allow(dead_code)] // Reserved for the OAuth refresh flow (Phase 19+).
pub const TYPE_TOKEN_EXPIRED: &str = "token_expired";
#[allow(dead_code)] // Reserved for future search-term notifications.
pub const TYPE_NEW_SEARCH_TERM: &str = "new_search_term";
pub const TYPE_SYSTEM_UPDATE: &str = "system_update";

/// Dedupe window for the in-process recent-failures cache used by
/// [`dispatch_ytdlp_failure_deduped`].
const YTDLP_FAILURE_DEDUP: Duration = Duration::from_secs(24 * 60 * 60);

/// One day in seconds — used by the per-day dedup helpers.
const ONE_DAY_SECS: i64 = 24 * 60 * 60;

/// Insert a single notification for one parent.
///
/// `metadata` is serialised to JSON; pass `&serde_json::Value::Null` (or
/// any small struct) when you have nothing structured to attach.
pub async fn dispatch<T: Serialize>(
    pool: &SqlitePool,
    parent_id: i64,
    notification_type: &str,
    title: &str,
    message: &str,
    metadata: &T,
) -> AppResult<()> {
    let metadata_json = serde_json::to_string(metadata).unwrap_or_else(|_| "null".to_string());
    sqlx::query(
        "INSERT INTO parent_notifications \
            (parent_account_id, notification_type, title, message, metadata) \
         VALUES (?, ?, ?, ?, ?)",
    )
    .bind(parent_id)
    .bind(notification_type)
    .bind(title)
    .bind(message)
    .bind(&metadata_json)
    .execute(pool)
    .await?;
    Ok(())
}

/// Dispatch the same notification to every parent account.
pub async fn broadcast<T: Serialize>(
    pool: &SqlitePool,
    notification_type: &str,
    title: &str,
    message: &str,
    metadata: &T,
) -> AppResult<()> {
    let parents: Vec<(i64,)> =
        sqlx::query_as("SELECT id FROM accounts WHERE account_type = 'parent'")
            .fetch_all(pool)
            .await?;
    for (parent_id,) in parents {
        dispatch(pool, parent_id, notification_type, title, message, metadata).await?;
    }
    Ok(())
}

/// Broadcast a notification to every parent **at most once per
/// `notification_type` + dedup-key combination** within the past
/// `window_seconds` seconds.
///
/// Dedup is implemented by pattern-matching `dedup_key` against the
/// stored `metadata` JSON column (with a SQL `LIKE`). Callers are
/// responsible for ensuring `dedup_key` is a substring that uniquely
/// identifies the notification — the simplest pattern is a JSON
/// fragment such as `"\"child_account_id\":42"`.
///
/// Set `window_seconds = ONE_DAY_SECS` for the common "one per day"
/// case.
pub async fn broadcast_once_within<T: Serialize>(
    pool: &SqlitePool,
    notification_type: &str,
    dedup_key: &str,
    window_seconds: i64,
    title: &str,
    message: &str,
    metadata: &T,
) -> AppResult<()> {
    let parents: Vec<(i64,)> =
        sqlx::query_as("SELECT id FROM accounts WHERE account_type = 'parent'")
            .fetch_all(pool)
            .await?;
    let metadata_json = serde_json::to_string(metadata).unwrap_or_else(|_| "null".to_string());
    let pattern = format!("%{dedup_key}%");
    for (parent_id,) in parents {
        let exists: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM parent_notifications \
             WHERE parent_account_id = ? \
               AND notification_type = ? \
               AND created_at >= unixepoch() - ? \
               AND COALESCE(metadata, '') LIKE ?",
        )
        .bind(parent_id)
        .bind(notification_type)
        .bind(window_seconds)
        .bind(&pattern)
        .fetch_one(pool)
        .await
        .unwrap_or(0);
        if exists > 0 {
            continue;
        }
        sqlx::query(
            "INSERT INTO parent_notifications \
                (parent_account_id, notification_type, title, message, metadata) \
             VALUES (?, ?, ?, ?, ?)",
        )
        .bind(parent_id)
        .bind(notification_type)
        .bind(title)
        .bind(message)
        .bind(&metadata_json)
        .execute(pool)
        .await?;
    }
    Ok(())
}

/// Convenience: broadcast at most once per (`notification_type`,
/// `dedup_key`) within a 24-hour window. The most common pattern.
pub async fn broadcast_once_per_day<T: Serialize>(
    pool: &SqlitePool,
    notification_type: &str,
    dedup_key: &str,
    title: &str,
    message: &str,
    metadata: &T,
) -> AppResult<()> {
    broadcast_once_within(
        pool,
        notification_type,
        dedup_key,
        ONE_DAY_SECS,
        title,
        message,
        metadata,
    )
    .await
}

/// Build a JSON-fragment dedup key like `"\"key\":value"` that can be
/// matched against the `metadata` column with a `LIKE` substring search.
///
/// The output is guaranteed to round-trip safely against the JSON form
/// `serde_json::to_string` produces — namely, `serde_json` never inserts
/// whitespace around the colon for object keys.
pub fn json_fragment_key(key: &str, value: &impl Serialize) -> String {
    let value_json = serde_json::to_string(value).unwrap_or_else(|_| "null".to_string());
    format!("\"{key}\":{value_json}")
}

/// Dispatch a yt-dlp failure for a specific video, deduped against an
/// in-process cache so a single bad video doesn't spam parents on every
/// page load.
///
/// We only record one failure per `video_id` per
/// [`YTDLP_FAILURE_DEDUP`] window. The cache is also pruned in-line so
/// it doesn't grow unboundedly.
pub async fn dispatch_ytdlp_failure_deduped(
    pool: &SqlitePool,
    video_id: &str,
    error_message: &str,
) -> AppResult<()> {
    if !should_dispatch_ytdlp_failure(video_id).await {
        return Ok(());
    }
    let payload = serde_json::json!({
        "video_id": video_id,
        "error": error_message,
    });
    broadcast(
        pool,
        TYPE_YTDLP_FAILURE,
        "Video extraction failed",
        &format!("Could not load metadata for video {video_id}."),
        &payload,
    )
    .await
}

async fn should_dispatch_ytdlp_failure(video_id: &str) -> bool {
    static CACHE: OnceLock<Mutex<HashMap<String, Instant>>> = OnceLock::new();
    let cache = CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    let mut guard = cache.lock().await;
    let now = Instant::now();
    // Drop expired entries first so the map can't grow without bound.
    guard.retain(|_, ts| now.duration_since(*ts) < YTDLP_FAILURE_DEDUP);
    if let Some(prev) = guard.get(video_id) {
        if now.duration_since(*prev) < YTDLP_FAILURE_DEDUP {
            return false;
        }
    }
    guard.insert(video_id.to_string(), now);
    true
}
