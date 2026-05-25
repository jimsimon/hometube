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
pub const TYPE_YTDLP_FAILURE: &str = "ytdlp_failure";
pub const TYPE_NEW_SEARCH_TERM: &str = "new_search_term";
pub const TYPE_SYSTEM_UPDATE: &str = "system_update";
pub const TYPE_CHANNEL_BACKFILL_ERROR: &str = "channel_backfill_error";

/// Dedupe window for the in-process recent-failures cache used by
/// [`dispatch_ytdlp_failure_deduped`].
const YTDLP_FAILURE_DEDUP: Duration = Duration::from_secs(24 * 60 * 60);

/// Dedupe window for the in-process recent-shelve cache used by
/// [`dispatch_channel_backfill_error_deduped`]. Mirrors yt-dlp's
/// 24-hour window so the parent notification bell doesn't churn while
/// the operator is investigating.
const CHANNEL_BACKFILL_ERROR_DEDUP: Duration = Duration::from_secs(24 * 60 * 60);

/// One day in seconds — used by the per-day dedup helpers.
const ONE_DAY_SECS: i64 = 24 * 60 * 60;

/// Low-level insert. Does **not** forward to external services — that
/// is the caller's responsibility, since broadcast-style callers want
/// exactly one external delivery for N row inserts.
async fn insert_one(
    pool: &SqlitePool,
    parent_id: i64,
    notification_type: &str,
    title: &str,
    message: &str,
    metadata_json: &str,
) -> AppResult<()> {
    sqlx::query(
        "INSERT INTO parent_notifications \
            (parent_account_id, notification_type, title, message, metadata) \
         VALUES (?, ?, ?, ?, ?)",
    )
    .bind(parent_id)
    .bind(notification_type)
    .bind(title)
    .bind(message)
    .bind(metadata_json)
    .execute(pool)
    .await?;
    Ok(())
}

/// Insert a single notification for one parent.
///
/// `metadata` is serialised to JSON; pass `&serde_json::Value::Null` (or
/// any small struct) when you have nothing structured to attach.
///
/// **External delivery**: after the in-app row is persisted this also
/// triggers a single fire-and-forget push via
/// [`crate::services::notification_forwarders::forward_if_enabled`] (if
/// a self-hosted forwarder is configured). Callers that fan out via
/// [`broadcast`] / [`broadcast_once_within`] must **not** loop over
/// `dispatch` — use [`insert_one`] directly to avoid N external pushes
/// for one logical notification.
pub async fn dispatch<T: Serialize>(
    pool: &SqlitePool,
    parent_id: i64,
    notification_type: &str,
    title: &str,
    message: &str,
    metadata: &T,
) -> AppResult<()> {
    let metadata_json = serde_json::to_string(metadata).unwrap_or_else(|_| "null".to_string());
    insert_one(
        pool,
        parent_id,
        notification_type,
        title,
        message,
        &metadata_json,
    )
    .await?;
    crate::services::notification_forwarders::forward_if_enabled(
        pool,
        notification_type,
        title,
        message,
    )
    .await;
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
    let metadata_json = serde_json::to_string(metadata).unwrap_or_else(|_| "null".to_string());
    let parents: Vec<(i64,)> =
        sqlx::query_as("SELECT id FROM accounts WHERE account_type = 'parent'")
            .fetch_all(pool)
            .await?;
    for (parent_id,) in parents {
        insert_one(
            pool,
            parent_id,
            notification_type,
            title,
            message,
            &metadata_json,
        )
        .await?;
    }
    crate::services::notification_forwarders::forward_if_enabled(
        pool,
        notification_type,
        title,
        message,
    )
    .await;
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
    let mut inserted_any = false;
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
        insert_one(
            pool,
            parent_id,
            notification_type,
            title,
            message,
            &metadata_json,
        )
        .await?;
        inserted_any = true;
    }
    if inserted_any {
        crate::services::notification_forwarders::forward_if_enabled(
            pool,
            notification_type,
            title,
            message,
        )
        .await;
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

/// Notify every parent that a child entered a search term that has not
/// been observed before for that child. Deduped to one notification per
/// (child, query) per day so reloading the same search doesn't repeat.
pub async fn dispatch_new_search_term(
    pool: &SqlitePool,
    child_id: i64,
    child_display_name: &str,
    query: &str,
) -> AppResult<()> {
    let metadata = serde_json::json!({
        "child_account_id": child_id,
        "query": query,
    });
    // Build a dedup key combining child + query JSON fragments. We
    // can't rely on stable key ordering inside the metadata column
    // (serde_json sorts object keys alphabetically), so we compose
    // two single-key fragments and let `LIKE` find both.
    let dedup_key = format!(
        "{}%{}",
        json_fragment_key("child_account_id", &child_id),
        json_fragment_key("query", &query),
    );
    broadcast_once_per_day(
        pool,
        TYPE_NEW_SEARCH_TERM,
        &dedup_key,
        "New search term",
        &format!("{child_display_name} searched for: {query}"),
        &metadata,
    )
    .await
}

/// Notify every parent that yt-dlp has been upgraded to a new version.
/// Deduped per `new_version` per day.
pub async fn dispatch_ytdlp_upgraded(
    pool: &SqlitePool,
    old_version: Option<&str>,
    new_version: &str,
) -> AppResult<()> {
    let metadata = serde_json::json!({
        "kind": "ytdlp_upgraded",
        "old_version": old_version,
        "new_version": new_version,
    });
    // Combine `kind` and `new_version` fragments so other
    // `system_update` notifications (e.g. PIN failures) don't
    // collide with yt-dlp upgrade dedup.
    let dedup_key = format!(
        "{}%{}",
        json_fragment_key("kind", &"ytdlp_upgraded"),
        json_fragment_key("new_version", &new_version),
    );
    broadcast_once_per_day(
        pool,
        TYPE_SYSTEM_UPDATE,
        &dedup_key,
        "yt-dlp updated",
        &match old_version {
            Some(old) => format!("yt-dlp upgraded from {old} to {new_version}."),
            None => format!("yt-dlp installed at version {new_version}."),
        },
        &metadata,
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

/// Dispatch a channel-backfill shelve notification, deduped against an
/// in-process cache so a single bad channel doesn't spam parents on
/// every retry tick.
///
/// Called only when the backfill loop transitions a channel to
/// `backfill_status='shelved'` after 5 consecutive failures — not on
/// every failure. Deduped per `channel_id` per
/// [`CHANNEL_BACKFILL_ERROR_DEDUP`] window so an operator who clears
/// the shelved state and watches it re-shelve doesn't see two pings.
pub async fn dispatch_channel_backfill_error_deduped(
    pool: &SqlitePool,
    channel_id: &str,
    channel_title: Option<&str>,
    error_message: &str,
) -> AppResult<()> {
    if !should_dispatch_channel_backfill_error(channel_id).await {
        return Ok(());
    }
    let display = channel_title.unwrap_or(channel_id);
    let payload = serde_json::json!({
        "channel_id": channel_id,
        "channel_title": channel_title,
        "error": error_message,
    });
    broadcast(
        pool,
        TYPE_CHANNEL_BACKFILL_ERROR,
        "Channel backfill failed",
        &format!(
            "Could not refresh the video archive for \"{display}\" after repeated attempts. \
             The channel has been shelved until you clear it from the parent settings page."
        ),
        &payload,
    )
    .await
}

async fn should_dispatch_channel_backfill_error(channel_id: &str) -> bool {
    static CACHE: OnceLock<Mutex<HashMap<String, Instant>>> = OnceLock::new();
    let cache = CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    let mut guard = cache.lock().await;
    let now = Instant::now();
    guard.retain(|_, ts| now.duration_since(*ts) < CHANNEL_BACKFILL_ERROR_DEDUP);
    if let Some(prev) = guard.get(channel_id) {
        if now.duration_since(*prev) < CHANNEL_BACKFILL_ERROR_DEDUP {
            return false;
        }
    }
    guard.insert(channel_id.to_string(), now);
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_fragment_key_emits_unspaced_pair() {
        let key = json_fragment_key("child_account_id", &42);
        assert_eq!(key, "\"child_account_id\":42");
    }

    #[test]
    fn json_fragment_key_handles_strings() {
        let key = json_fragment_key("video_id", &"abc");
        assert_eq!(key, "\"video_id\":\"abc\"");
    }
}
