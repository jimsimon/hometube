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
// Note: `can_child_view` is no longer needed here — `list_videos`
// applies its blocked/hidden filters inline in the main SQL query
// (which simultaneously fixes the prior pagination bug). Channel-level
// access is still gated by `enforce_channel_access`.
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
///
/// **Access control** lives in the SQL: blocked + hidden videos are
/// excluded by `NOT EXISTS` subqueries in the main query, mirroring
/// `feed_for_child`. We previously filtered post-fetch via
/// `can_child_view`, which had two bugs:
///   1. **N+1 queries** — up to 4 round-trips per row × `PAGE_SIZE`
///      rows = ~120 queries per page.
///   2. **Broken pagination** — `next_page_token` was emitted iff the
///      *filtered* page was full, so any blocked/hidden video silently
///      truncated the listing (`items.len() < PAGE_SIZE` ⇒ no more
///      pages, even when thousands more were available).
/// `enforce_channel_access` at the top of the handler still gates
/// channel-level access; per-video allowlist is implicit since we
/// only read from this channel's rows.
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

    // Whitelist `sort` so typos surface as a clear 400 instead of
    // silently degrading to `latest` with no warning. The values
    // accepted here must match the CASE expression in the SQL below.
    let sort = match q.sort.as_deref().unwrap_or("latest") {
        "latest" | "most_viewed" => q.sort.as_deref().unwrap_or("latest").to_string(),
        other => {
            return Err(AppError::BadRequest(format!(
                "unknown sort '{other}' — accepted values: latest, most_viewed"
            )));
        }
    };

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

    // Single query: filter tombstones + blocked + hidden inline so
    // `LIMIT n` returns at most `n` actually-renderable rows. The
    // `NOT EXISTS` pattern is the same one `feed_for_child` uses
    // (`src/services/feed_cache.rs:feed_for_child`).
    //
    // The ORDER BY uses a CASE on the requested sort so we don't have
    // to fork two queries:
    //   - `most_viewed` → sort by view_count DESC (nulls last via
    //     COALESCE(-1)), then published_at DESC as a tiebreaker.
    //   - anything else → sort by published_at DESC, last_seen_at DESC.
    let rows: Vec<Row> = sqlx::query_as(
        "SELECT cv.video_id, cv.title, cv.channel_id, cv.channel_title, cv.thumbnail_url, \
                cv.published_at, cv.duration_s, cv.view_count \
           FROM channel_videos cv \
          WHERE cv.channel_id = ?1 \
            AND cv.is_deleted = 0 \
            AND NOT EXISTS ( \
                SELECT 1 FROM blocked_videos b \
                 WHERE b.child_account_id = ?2 AND b.video_id = cv.video_id) \
            AND NOT EXISTS ( \
                SELECT 1 FROM hidden_videos h \
                 WHERE h.child_account_id = ?2 AND h.video_id = cv.video_id) \
          ORDER BY \
              CASE WHEN ?3 = 'most_viewed' THEN COALESCE(cv.view_count, -1) ELSE 0 END DESC, \
              cv.published_at DESC, \
              cv.last_seen_at DESC \
          LIMIT ?4 OFFSET ?5",
    )
    .bind(&channel_id)
    .bind(current.id)
    .bind(&sort)
    .bind(PAGE_SIZE as i64)
    .bind(cursor)
    .fetch_all(&state.db)
    .await?;

    // Capture the row count BEFORE adapting, so pagination is driven
    // by what the DB returned (post-filter) rather than by what we
    // hand back to the client.
    let fetched = rows.len() as u32;
    let items: Vec<ChannelVideoItem> = rows
        .into_iter()
        .map(|r| ChannelVideoItem {
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
        })
        .collect();

    // Emit a next_page_token whenever the DB filled the page. The
    // server has already applied is_deleted / blocked / hidden filters
    // in SQL, so a full page from the DB really is a full client page.
    let next_page_token = if fetched >= PAGE_SIZE {
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
