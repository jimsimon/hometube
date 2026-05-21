//! Content access control.
//!
//! Centralises the "is this child allowed to watch this video?" decision:
//!
//! ```text
//! Is video blocked for this child? → Yes → Deny (403)
//! Is video in allowlisted videos?  → Yes → Allow
//! Is video from an allowlisted channel? → Yes → Allow
//! → Deny (403)
//! ```

use sqlx::SqlitePool;

use crate::error::AppResult;

/// Decide whether `child_id` may view `video_id`.
///
/// `channel_id` may be `None` if the caller could not determine it
/// (e.g., yt-dlp didn't expose a `channel_id`); in that case the
/// decision falls back to direct video allowlisting.
pub async fn can_child_view(
    pool: &SqlitePool,
    child_id: i64,
    video_id: &str,
    channel_id: Option<&str>,
) -> AppResult<bool> {
    // Blocked overrides everything.
    if is_blocked(pool, child_id, video_id).await? {
        return Ok(false);
    }
    // A child's personal "hide" also denies access — the video should
    // not surface anywhere except the dedicated `/child/hidden` page.
    if is_hidden_for_child(pool, child_id, video_id).await? {
        return Ok(false);
    }
    if is_allowlisted_video(pool, child_id, video_id).await? {
        return Ok(true);
    }
    if let Some(ch) = channel_id {
        if is_allowlisted_channel(pool, child_id, ch).await? {
            return Ok(true);
        }
    }
    Ok(false)
}

/// True if this child has personally hidden this video.
pub async fn is_hidden_for_child(
    pool: &SqlitePool,
    child_id: i64,
    video_id: &str,
) -> AppResult<bool> {
    let row: (i64,) = sqlx::query_as(
        "SELECT COUNT(*) FROM hidden_videos WHERE child_account_id = ? AND video_id = ?",
    )
    .bind(child_id)
    .bind(video_id)
    .fetch_one(pool)
    .await?;
    Ok(row.0 > 0)
}

async fn is_blocked(pool: &SqlitePool, child_id: i64, video_id: &str) -> AppResult<bool> {
    let row: (i64,) = sqlx::query_as(
        "SELECT COUNT(*) FROM blocked_videos WHERE child_account_id = ? AND video_id = ?",
    )
    .bind(child_id)
    .bind(video_id)
    .fetch_one(pool)
    .await?;
    Ok(row.0 > 0)
}

async fn is_allowlisted_video(pool: &SqlitePool, child_id: i64, video_id: &str) -> AppResult<bool> {
    let row: (i64,) = sqlx::query_as(
        "SELECT COUNT(*) FROM allowlisted_videos WHERE child_account_id = ? AND video_id = ?",
    )
    .bind(child_id)
    .bind(video_id)
    .fetch_one(pool)
    .await?;
    Ok(row.0 > 0)
}

async fn is_allowlisted_channel(
    pool: &SqlitePool,
    child_id: i64,
    channel_id: &str,
) -> AppResult<bool> {
    let row: (i64,) = sqlx::query_as(
        "SELECT COUNT(*) FROM allowlisted_channels WHERE child_account_id = ? AND channel_id = ?",
    )
    .bind(child_id)
    .bind(channel_id)
    .fetch_one(pool)
    .await?;
    Ok(row.0 > 0)
}

/// Confirm `account_id` is a child account; returns false otherwise.
/// Used by parent-only routes that take a `:id` path parameter and need
/// to refuse parent IDs.
pub async fn is_child_account(pool: &SqlitePool, account_id: i64) -> AppResult<bool> {
    let row: Option<(String,)> = sqlx::query_as("SELECT account_type FROM accounts WHERE id = ?")
        .bind(account_id)
        .fetch_optional(pool)
        .await?;
    Ok(matches!(row, Some((t,)) if t == "child"))
}

/// Look up the allowlisted channel IDs for the child.
pub async fn child_allowlisted_channel_ids(
    pool: &SqlitePool,
    child_id: i64,
) -> AppResult<Vec<String>> {
    let rows: Vec<(String,)> =
        sqlx::query_as("SELECT channel_id FROM allowlisted_channels WHERE child_account_id = ?")
            .bind(child_id)
            .fetch_all(pool)
            .await?;
    Ok(rows.into_iter().map(|(c,)| c).collect())
}
