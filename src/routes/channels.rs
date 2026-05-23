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
use std::collections::HashMap;

use crate::error::{AppError, AppResult};
use crate::middleware::auth::CurrentAccount;
use crate::routes::search::{decode_page_token, encode_page_token, PageCursor};
use crate::services::access::can_child_view;
use crate::services::youtube::{ChannelInfo, ChannelVideoItem, ThumbnailInfo};
use crate::state::AppState;

/// Default page size when paging through a channel's videos.
const PAGE_SIZE: u32 = 30;

/// `GET /api/channels/:channelId` — return channel metadata for an
/// allowed channel, served entirely from `channel_sync_state`. Zero
/// YouTube calls — the metadata was either forwarded from the parent
/// search response at allowlist time or filled in by the sidecar
/// fallback on raw-paste adds.
pub async fn get_channel(
    State(state): State<AppState>,
    current: CurrentAccount,
    Path(channel_id): Path<String>,
) -> AppResult<Json<ChannelInfo>> {
    enforce_channel_access(&state, current.id, &channel_id).await?;

    let row: Option<(String, Option<String>, Option<String>, Option<String>)> = sqlx::query_as(
        "SELECT channel_id, channel_title, channel_thumbnail_url, description \
           FROM channel_sync_state WHERE channel_id = ?",
    )
    .bind(&channel_id)
    .fetch_optional(&state.db)
    .await?;
    let (id, title, thumbnail_url, description) = row.ok_or(AppError::NotFound)?;

    // video_count is computed live so it matches what the child
    // actually sees (excludes tombstones).
    let video_count: Option<i64> = sqlx::query_scalar(
        "SELECT COUNT(*) FROM channel_videos \
           WHERE channel_id = ? AND is_deleted = 0",
    )
    .bind(&channel_id)
    .fetch_one(&state.db)
    .await
    .ok();

    let mut thumbnails: HashMap<String, ThumbnailInfo> = HashMap::new();
    if let Some(url) = thumbnail_url {
        thumbnails.insert(
            "default".into(),
            ThumbnailInfo {
                url,
                width: None,
                height: None,
            },
        );
    }

    Ok(Json(ChannelInfo {
        id,
        title: title.unwrap_or_default(),
        description: description.unwrap_or_default(),
        thumbnails,
        video_count,
    }))
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
    pub items: Vec<ChannelVideoItem>,
    pub next_page_token: Option<String>,
}

/// `GET /api/channels/:channelId/videos`.
///
/// Pulls a page of the channel's archive directly from `channel_videos`
/// (no YouTube round-trip per request). Offset-based pagination via
/// the shared `PageCursor` type; the response shape (`items` +
/// `next_page_token`) matches the previous sidecar-backed version
/// for frontend compatibility.
///
/// `sort=most_viewed` orders by `view_count DESC NULLS LAST` (populated
/// by the yt-dlp backfill); RSS-only rows with NULL view_count sort to
/// the bottom via `COALESCE(view_count, -1)`. The default `latest` sort
/// is by `published_at DESC, last_seen_at DESC`.
pub async fn list_videos(
    State(state): State<AppState>,
    current: CurrentAccount,
    Path(channel_id): Path<String>,
    Query(q): Query<ListVideosQuery>,
) -> AppResult<Json<ChannelVideosPage>> {
    enforce_channel_access(&state, current.id, &channel_id).await?;

    let cursor = q
        .page_token
        .as_deref()
        .and_then(decode_page_token)
        .map(|c| c.offset)
        .unwrap_or(0);
    let sort = q.sort.as_deref().unwrap_or("latest").to_string();

    // We fetch one page worth of rows. The ORDER BY uses a CASE on
    // the requested sort so we don't have to fork two queries:
    //   - `most_viewed` → sort by view_count DESC (nulls last via
    //     COALESCE(-1)), then published_at DESC as a tiebreaker.
    //   - anything else → sort by published_at DESC, last_seen_at DESC.
    #[derive(sqlx::FromRow)]
    struct Row {
        video_id: String,
        title: String,
        channel_id: Option<String>,
        channel_title: Option<String>,
        thumbnail_url: Option<String>,
        published_at: Option<i64>,
        #[allow(dead_code)]
        duration_s: Option<i64>,
        #[allow(dead_code)]
        view_count: Option<i64>,
    }
    let rows: Vec<Row> = sqlx::query_as(
        "SELECT video_id, title, channel_id, channel_title, thumbnail_url, \
                published_at, duration_s, view_count \
           FROM channel_videos \
          WHERE channel_id = ? AND is_deleted = 0 \
          ORDER BY \
              CASE WHEN ? = 'most_viewed' THEN COALESCE(view_count, -1) ELSE 0 END DESC, \
              published_at DESC, \
              last_seen_at DESC \
          LIMIT ? OFFSET ?",
    )
    .bind(&channel_id)
    .bind(&sort)
    .bind(PAGE_SIZE as i64)
    .bind(cursor)
    .fetch_all(&state.db)
    .await?;

    // Adapt to ChannelVideoItem + run access control.
    let mut items: Vec<ChannelVideoItem> = Vec::with_capacity(rows.len());
    for r in rows {
        let video_id = r.video_id.clone();
        let row_channel_id = r.channel_id.clone();
        let item = ChannelVideoItem {
            video_id: r.video_id,
            title: r.title,
            channel_id: r.channel_id,
            channel_title: r.channel_title,
            thumbnails: r
                .thumbnail_url
                .map(|url| {
                    let mut m = HashMap::new();
                    m.insert(
                        "default".to_string(),
                        ThumbnailInfo {
                            url,
                            width: None,
                            height: None,
                        },
                    );
                    m
                })
                .unwrap_or_default(),
            published_at: r.published_at.map(|s| s.to_string()),
            position: None,
        };
        let allowed = can_child_view(
            &state.db,
            current.id,
            &video_id,
            row_channel_id.as_deref().or(Some(channel_id.as_str())),
        )
        .await
        .unwrap_or(false);
        if allowed {
            items.push(item);
        }
    }

    // Emit a next_page_token if we filled the page (might be more).
    let next_page_token = if items.len() as u32 >= PAGE_SIZE {
        Some(encode_page_token(&PageCursor {
            offset: cursor + PAGE_SIZE as i64,
        }))
    } else {
        None
    };

    Ok(Json(ChannelVideosPage {
        items,
        next_page_token,
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
