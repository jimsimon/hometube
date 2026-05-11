//! Child subscription routes.
//!
//! `child_subscriptions` is the local mirror of the child's YouTube
//! subscriptions. Three endpoints:
//!
//! - `GET /api/subscriptions` — list non-deleted subscriptions, with a
//!   `visible` flag computed by joining against
//!   `allowlisted_channels`. Hidden rows are still returned so the UI
//!   can grey them out and prompt the parent to allowlist them.
//! - `POST /api/subscriptions` — body `{ channel_id }`. Inserts (or
//!   un-soft-deletes) a row, marks it `pending_push`, and spawns a
//!   background task to call `subscriptions.insert` on YouTube via
//!   [`crate::services::sync::push_subscription_change`].
//! - `DELETE /api/subscriptions/:channelId` — soft-deletes the row,
//!   marks it `pending_delete`, and spawns the analogous push.

use axum::{
    extract::{Path, State},
    http::StatusCode,
    Json,
};
use serde::{Deserialize, Serialize};

use crate::error::{AppError, AppResult};
use crate::middleware::auth::CurrentAccount;
use crate::services::sync::push_subscription_change;
use crate::services::youtube::YoutubeClient;
use crate::state::AppState;

/// One row of the subscription list, as returned by `GET /api/subscriptions`.
#[derive(Debug, Serialize, sqlx::FromRow)]
pub struct SubscriptionRow {
    pub id: i64,
    pub channel_id: String,
    pub channel_title: String,
    pub channel_thumbnail_url: Option<String>,
    pub source: String,
    pub sync_status: String,
    pub subscribed_at: i64,
    /// `true` when the channel is in this child's allowlist; `false` when
    /// the subscription exists locally (or on YouTube) but the parent
    /// hasn't allowlisted it yet, in which case the UI should grey it
    /// out.
    pub visible: bool,
}

/// Tuple shape produced by [`list`]'s SELECT. Defined as an alias to
/// satisfy clippy's `type_complexity` lint.
///
/// Columns in order:
/// `id, channel_id, channel_title, channel_thumbnail_url, source,
///  sync_status, subscribed_at, visible`.
type SubscriptionRowTuple = (
    i64,
    String,
    String,
    Option<String>,
    String,
    String,
    i64,
    i64,
);

/// `GET /api/subscriptions`.
pub async fn list(
    State(state): State<AppState>,
    current: CurrentAccount,
) -> AppResult<Json<Vec<SubscriptionRow>>> {
    let rows: Vec<SubscriptionRowTuple> = sqlx::query_as(
        "SELECT s.id, s.channel_id, s.channel_title, s.channel_thumbnail_url, \
                    s.source, s.sync_status, s.subscribed_at, \
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
            |(
                id,
                channel_id,
                channel_title,
                channel_thumbnail_url,
                source,
                sync_status,
                subscribed_at,
                visible,
            )| SubscriptionRow {
                id,
                channel_id,
                channel_title,
                channel_thumbnail_url,
                source,
                sync_status,
                subscribed_at,
                visible: visible != 0,
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
/// Inserts (or revives a soft-deleted row) with `source='app'` and
/// `sync_status='pending_push'`, then spawns a background task to push
/// the change to YouTube. Returns the freshly-inserted row immediately
/// so the UI can render an optimistic "pending" pill.
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
             source, sync_status, is_deleted) \
         VALUES (?, ?, ?, ?, 'app', 'pending_push', 0) \
         ON CONFLICT(child_account_id, channel_id) DO UPDATE SET \
            channel_title = excluded.channel_title, \
            channel_thumbnail_url = excluded.channel_thumbnail_url, \
            source = 'app', \
            sync_status = 'pending_push', \
            is_deleted = 0, \
            updated_at = unixepoch()",
    )
    .bind(current.id)
    .bind(&info.id)
    .bind(&info.title)
    .bind(thumb)
    .execute(&state.db)
    .await?;

    spawn_sub_push(state.clone(), current.id, info.id.clone());

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
         SET is_deleted = 1, sync_status = 'pending_delete', updated_at = unixepoch() \
         WHERE child_account_id = ? AND channel_id = ?",
    )
    .bind(current.id)
    .bind(&channel_id)
    .execute(&state.db)
    .await?;
    if result.rows_affected() == 0 {
        return Err(AppError::NotFound);
    }
    spawn_sub_push(state.clone(), current.id, channel_id);
    Ok(StatusCode::NO_CONTENT)
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn spawn_sub_push(state: AppState, account_id: i64, channel_id: String) {
    tokio::spawn(async move {
        if let Err(err) = push_subscription_change(&state.db, account_id, &channel_id).await {
            tracing::warn!(account_id, %channel_id, %err, "subscription push failed");
        }
    });
}

async fn fetch_one(
    state: &AppState,
    child_id: i64,
    channel_id: &str,
) -> AppResult<SubscriptionRow> {
    let row: (
        i64,
        String,
        String,
        Option<String>,
        String,
        String,
        i64,
        i64,
    ) = sqlx::query_as(
        "SELECT s.id, s.channel_id, s.channel_title, s.channel_thumbnail_url, \
                s.source, s.sync_status, s.subscribed_at, \
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
    let (
        id,
        channel_id,
        channel_title,
        channel_thumbnail_url,
        source,
        sync_status,
        subscribed_at,
        visible,
    ) = row;
    Ok(SubscriptionRow {
        id,
        channel_id,
        channel_title,
        channel_thumbnail_url,
        source,
        sync_status,
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
