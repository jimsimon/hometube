//! Child subscription routes.
//!
//! `child_subscriptions` stores the child's channel subscriptions.
//! Three endpoints:
//!
//! - `GET /api/subscriptions` — list non-deleted subscriptions, with a
//!   `visible` flag computed by joining against
//!   `allowlisted_channels`. Hidden rows are still returned so the UI
//!   can grey them out and prompt the parent to allowlist them.
//! - `POST /api/subscriptions` — body `{ channel_id }`. Inserts (or
//!   un-soft-deletes) a row.
//! - `DELETE /api/subscriptions/:channelId` — soft-deletes the row.

use axum::{
    extract::{Path, State},
    http::StatusCode,
    Json,
};
use serde::{Deserialize, Serialize};

use crate::error::{AppError, AppResult};
use crate::middleware::auth::CurrentAccount;
use crate::services::youtube::YoutubeClient;
use crate::state::AppState;

/// One row of the subscription list, as returned by `GET /api/subscriptions`.
#[derive(Debug, Serialize, sqlx::FromRow)]
pub struct SubscriptionRow {
    pub id: i64,
    pub channel_id: String,
    pub channel_title: String,
    pub channel_thumbnail_url: Option<String>,
    pub subscribed_at: i64,
    /// `true` when the channel is in this child's allowlist; `false` when
    /// the subscription exists locally but the parent hasn't allowlisted
    /// it yet, in which case the UI should grey it out.
    pub visible: bool,
}

/// Tuple shape produced by [`list`]'s SELECT. Defined as an alias to
/// satisfy clippy's `type_complexity` lint.
///
/// Columns in order:
/// `id, channel_id, channel_title, channel_thumbnail_url,
///  subscribed_at, visible`.
type SubscriptionRowTuple = (i64, String, String, Option<String>, i64, i64);

/// `GET /api/subscriptions`.
pub async fn list(
    State(state): State<AppState>,
    current: CurrentAccount,
) -> AppResult<Json<Vec<SubscriptionRow>>> {
    let rows: Vec<SubscriptionRowTuple> = sqlx::query_as(
        "SELECT s.id, s.channel_id, s.channel_title, s.channel_thumbnail_url, \
                    s.subscribed_at, \
                    CASE WHEN a.id IS NOT NULL THEN 1 ELSE 0 END AS visible \
             FROM child_subscriptions s \
             LEFT JOIN allowlisted_channels a \
               ON a.child_account_id = s.child_account_id AND a.channel_id = s.channel_id \
             WHERE s.child_account_id = ? AND s.is_deleted = 0 \
             ORDER BY visible DESC, s.subscribed_at DESC",
    )
    .bind(current.id)
    .fetch_all(&state.db)
    .await?;

    let out = rows
        .into_iter()
        .map(
            |(id, channel_id, channel_title, channel_thumbnail_url, subscribed_at, visible)| {
                SubscriptionRow {
                    id,
                    channel_id,
                    channel_title,
                    channel_thumbnail_url,
                    subscribed_at,
                    visible: visible != 0,
                }
            },
        )
        .collect();
    Ok(Json(out))
}

#[derive(Debug, Deserialize)]
pub struct SubscribeBody {
    pub channel_id: String,
}

/// `POST /api/subscriptions` — subscribe to a channel.
///
/// Inserts (or revives a soft-deleted row).
/// Returns the freshly-inserted row immediately.
pub async fn subscribe(
    State(state): State<AppState>,
    current: CurrentAccount,
    Json(body): Json<SubscribeBody>,
) -> AppResult<Json<SubscriptionRow>> {
    // Resolve channel metadata so the row has a usable title + thumbnail
    // even if the YouTube push hasn't completed yet.
    let yt = YoutubeClient::from_db(&state.db).await?;
    let info = yt
        .get_channel(&body.channel_id)
        .await?
        .ok_or_else(|| AppError::BadRequest("channel not found on YouTube".into()))?;
    let thumb = preferred_thumbnail(&info.thumbnails);

    sqlx::query(
        "INSERT INTO child_subscriptions \
            (child_account_id, channel_id, channel_title, channel_thumbnail_url, \
             is_deleted) \
         VALUES (?, ?, ?, ?, 0) \
         ON CONFLICT(child_account_id, channel_id) DO UPDATE SET \
            channel_title = excluded.channel_title, \
            channel_thumbnail_url = excluded.channel_thumbnail_url, \
            is_deleted = 0, \
            updated_at = unixepoch()",
    )
    .bind(current.id)
    .bind(&info.id)
    .bind(&info.title)
    .bind(thumb)
    .execute(&state.db)
    .await?;

    let row = fetch_one(&state, current.id, &info.id).await?;
    Ok(Json(row))
}

/// `DELETE /api/subscriptions/:channelId` — unsubscribe.
pub async fn unsubscribe(
    State(state): State<AppState>,
    current: CurrentAccount,
    Path(channel_id): Path<String>,
) -> AppResult<StatusCode> {
    let result = sqlx::query(
        "UPDATE child_subscriptions \
         SET is_deleted = 1, updated_at = unixepoch() \
         WHERE child_account_id = ? AND channel_id = ?",
    )
    .bind(current.id)
    .bind(&channel_id)
    .execute(&state.db)
    .await?;
    if result.rows_affected() == 0 {
        return Err(AppError::NotFound);
    }
    Ok(StatusCode::NO_CONTENT)
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

async fn fetch_one(
    state: &AppState,
    child_id: i64,
    channel_id: &str,
) -> AppResult<SubscriptionRow> {
    let row: (i64, String, String, Option<String>, i64, i64) = sqlx::query_as(
        "SELECT s.id, s.channel_id, s.channel_title, s.channel_thumbnail_url, \
                s.subscribed_at, \
                CASE WHEN a.id IS NOT NULL THEN 1 ELSE 0 END AS visible \
         FROM child_subscriptions s \
         LEFT JOIN allowlisted_channels a \
           ON a.child_account_id = s.child_account_id AND a.channel_id = s.channel_id \
         WHERE s.child_account_id = ? AND s.channel_id = ?",
    )
    .bind(child_id)
    .bind(channel_id)
    .fetch_one(&state.db)
    .await?;
    let (id, channel_id, channel_title, channel_thumbnail_url, subscribed_at, visible) = row;
    Ok(SubscriptionRow {
        id,
        channel_id,
        channel_title,
        channel_thumbnail_url,
        subscribed_at,
        visible: visible != 0,
    })
}

fn preferred_thumbnail(
    thumbs: &std::collections::HashMap<String, crate::services::youtube::ThumbnailInfo>,
) -> Option<String> {
    for key in ["maxres", "high", "standard", "medium", "default"] {
        if let Some(t) = thumbs.get(key) {
            return Some(t.url.clone());
        }
    }
    None
}
