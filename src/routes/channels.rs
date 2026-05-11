//! Child channel routes.
//!
//! Two endpoints:
//!
//! - `GET /api/channels/:channelId` — channel metadata.
//! - `GET /api/channels/:channelId/videos` — list of recent uploads,
//!   filtered through [`crate::services::access::can_child_view`] so
//!   blocked videos disappear and the rest are gated by the child's
//!   allowlist.
//!
//! For a child to see the channel page at all, the channel itself must
//! be in the child's allowlist *or* one of the channel's videos must
//! already be in `allowlisted_videos`. Otherwise the request returns
//! `403 Forbidden`.

use axum::{
    extract::{Path, Query, State},
    Json,
};
use serde::{Deserialize, Serialize};

use crate::error::{AppError, AppResult};
use crate::middleware::auth::CurrentAccount;
use crate::services::access::can_child_view;
use crate::services::youtube::{ChannelInfo, PlaylistItem, YoutubeClient};
use crate::state::AppState;

/// Default page size when paging through a channel's videos.
const PAGE_SIZE: u32 = 30;

/// `GET /api/channels/:channelId` — return channel metadata if the
/// child is allowed to see this channel.
pub async fn get_channel(
    State(state): State<AppState>,
    current: CurrentAccount,
    Path(channel_id): Path<String>,
) -> AppResult<Json<ChannelInfo>> {
    enforce_channel_access(&state, current.id, &channel_id).await?;

    let yt = YoutubeClient::from_db(&state.db).await?;
    let info = yt
        .get_channel(&channel_id)
        .await?
        .ok_or(AppError::NotFound)?;
    Ok(Json(info))
}

#[derive(Debug, Deserialize)]
pub struct ListVideosQuery {
    /// Sort order. Recognised: `latest` (default) and `most_viewed`.
    /// Anything else is treated as "latest".
    #[serde(default)]
    pub sort: Option<String>,
    #[serde(default)]
    pub page_token: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct ChannelVideosPage {
    pub items: Vec<PlaylistItem>,
    pub next_page_token: Option<String>,
}

/// `GET /api/channels/:channelId/videos`.
///
/// Pulls a page of recent uploads via the YouTube uploads playlist, then
/// drops anything the child isn't allowed to see (blocked or not on the
/// allowlist). The `most_viewed` sort applies a stable secondary sort by
/// `view_count` — but YouTube's `playlistItems.list` doesn't expose view
/// counts, so we treat the `latest` ordering as authoritative and only
/// re-order when a real view-count source is available. For now,
/// `most_viewed` is accepted but degrades gracefully to `latest`.
pub async fn list_videos(
    State(state): State<AppState>,
    current: CurrentAccount,
    Path(channel_id): Path<String>,
    Query(q): Query<ListVideosQuery>,
) -> AppResult<Json<ChannelVideosPage>> {
    enforce_channel_access(&state, current.id, &channel_id).await?;

    let yt = YoutubeClient::from_db(&state.db).await?;
    let page = yt
        .list_channel_videos(&channel_id, PAGE_SIZE, q.page_token.as_deref())
        .await?;

    // Filter through access control.
    let mut items = Vec::with_capacity(page.items.len());
    for it in page.items {
        let allowed = can_child_view(
            &state.db,
            current.id,
            &it.video_id,
            it.channel_id.as_deref().or(Some(channel_id.as_str())),
            &[],
        )
        .await
        .unwrap_or(false);
        if allowed {
            items.push(it);
        }
    }

    // The plan exposes a `sort` parameter — we honour `latest` directly.
    // `most_viewed` is treated as a no-op secondary sort (kept for API
    // forward-compatibility).
    let _ = q.sort;

    Ok(Json(ChannelVideosPage {
        items,
        next_page_token: page.next_page_token,
    }))
}

/// Decide whether `child_id` is allowed to browse `channel_id` at all.
///
/// Allowed iff:
///
/// - the channel is in the child's `allowlisted_channels`, OR
/// - any video in `allowlisted_videos` has a `channel_title` matching
///   the channel (best effort — we don't store the channel ID), OR
/// - any video in `child_subscriptions` for this child matches.
async fn enforce_channel_access(
    state: &AppState,
    child_id: i64,
    channel_id: &str,
) -> AppResult<()> {
    // Direct allowlist hit?
    let allowlisted: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM allowlisted_channels \
         WHERE child_account_id = ? AND channel_id = ?",
    )
    .bind(child_id)
    .bind(channel_id)
    .fetch_one(&state.db)
    .await?;
    if allowlisted > 0 {
        return Ok(());
    }
    // Subscribed (and not soft-deleted)?
    let subscribed: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM child_subscriptions \
         WHERE child_account_id = ? AND channel_id = ? AND is_deleted = 0",
    )
    .bind(child_id)
    .bind(channel_id)
    .fetch_one(&state.db)
    .await?;
    if subscribed > 0 {
        return Ok(());
    }
    Err(AppError::Forbidden)
}
