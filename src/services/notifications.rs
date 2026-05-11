//! Parent-notification dispatcher.
//!
//! All notification inserts in the codebase should funnel through
//! [`dispatch`] (or its higher-level helpers) so we have a single place
//! to control:
//!
//! - validating the notification type against the `parent_notifications`
//!   schema CHECK constraint;
//! - persisting metadata as a JSON string;
//! - applying simple in-process dedup for "one per child per day"
//!   notifications without polluting handlers with that boilerplate.
//!
//! The existing Phase 11 / Phase 12 / Phase 15 callers still write
//! directly to the table; this module exposes equivalent helpers so
//! they can migrate in a follow-up.

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
pub const TYPE_TOKEN_EXPIRED: &str = "token_expired";
pub const TYPE_NEW_SEARCH_TERM: &str = "new_search_term";
pub const TYPE_SYSTEM_UPDATE: &str = "system_update";

/// Dedupe window for the in-process recent-failures cache used by
/// [`dispatch_ytdlp_failure_deduped`].
const YTDLP_FAILURE_DEDUP: Duration = Duration::from_secs(24 * 60 * 60);

/// Insert a single notification for one parent.
///
/// `metadata` is serialised to JSON; pass `serde_json::Value::Null` (or
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
