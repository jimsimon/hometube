//! Search helpers.
//!
//! Two distinct concerns share this module:
//!
//! - `GET /api/parent/search` — parent-side discovery, used by the
//!   allowlist UI to find content to add. Backed by the discovery
//!   sidecar. Implemented by [`parent_search`].
//! - `GET /api/search` — child-side allowlist-bounded search.
//!   Implemented by [`child_search`] (Phase 10). The child can only ever
//!   see content that is reachable from their allowlist, and every
//!   query is logged to `search_log` for parent visibility.

use axum::{
    extract::{Query, State},
    Json,
};
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine as _;
use serde::{Deserialize, Serialize};

use crate::error::{AppError, AppResult};
use crate::middleware::auth::CurrentAccount;
use crate::services::access::can_child_view;
use crate::services::youtube::{SearchItem, SearchType, ThumbnailInfo, YoutubeClient};
use crate::state::AppState;

/// Default number of items returned per type.
const DEFAULT_LIMIT: u32 = 20;
/// Hard cap to keep result payloads small.
const MAX_LIMIT: u32 = 50;

#[derive(Debug, Deserialize)]
pub struct ParentSearchQuery {
    pub q: String,
    /// `"channel"` or `"video"`.
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(default)]
    pub max_results: Option<u32>,
}

#[derive(Debug, Serialize)]
pub struct SearchResponse {
    pub items: Vec<SearchItem>,
}

/// `GET /api/parent/search?q=&type=channel|video`.
pub async fn parent_search(
    State(state): State<AppState>,
    Query(q): Query<ParentSearchQuery>,
) -> AppResult<Json<SearchResponse>> {
    let kind = SearchType::parse(&q.kind)
        .ok_or_else(|| AppError::BadRequest("type must be channel|video".into()))?;
    let yt = YoutubeClient::from_db(&state.db).await?;
    let items = yt.search(&q.q, kind, q.max_results.unwrap_or(15)).await?;
    Ok(Json(SearchResponse { items }))
}

// ---------------------------------------------------------------------------
// Child search
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct ChildSearchQuery {
    pub q: String,
    /// One of `channel`, `video`, or `all` (default).
    #[serde(default, rename = "type")]
    pub kind: Option<String>,
    /// Optional pagination cursor returned in a previous response's
    /// `next_page_token` field. The token is an opaque base64url-encoded
    /// JSON object of the form `{"offset": N}` and is applied uniformly
    /// to every result bucket (channels / videos). When absent we
    /// start at offset 0.
    #[serde(default)]
    pub page_token: Option<String>,
    #[serde(default)]
    pub limit: Option<u32>,
}

/// Decoded form of `page_token`. Shared with `channels::list_videos`
/// via the `pub(crate)` visibility so the two paginated endpoints
/// produce the same opaque-token shape.
#[derive(Debug, Serialize, Deserialize, Default)]
pub(crate) struct PageCursor {
    /// Number of rows already returned in earlier pages.
    pub(crate) offset: i64,
}

pub(crate) fn decode_page_token(token: &str) -> Option<PageCursor> {
    let decoded = URL_SAFE_NO_PAD.decode(token.as_bytes()).ok()?;
    serde_json::from_slice(&decoded).ok()
}

pub(crate) fn encode_page_token(cursor: &PageCursor) -> String {
    let json = serde_json::to_vec(cursor).unwrap_or_else(|_| b"{}".to_vec());
    URL_SAFE_NO_PAD.encode(json)
}

#[derive(Debug, Serialize, Clone)]
pub struct ChildChannelHit {
    pub channel_id: String,
    pub channel_title: String,
    pub channel_thumbnail_url: Option<String>,
}

#[derive(Debug, Serialize, Clone)]
pub struct ChildVideoHit {
    pub video_id: String,
    pub title: String,
    pub channel_id: Option<String>,
    pub channel_title: Option<String>,
    pub thumbnail_url: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct ChildSearchResults {
    pub channels: Vec<ChildChannelHit>,
    pub videos: Vec<ChildVideoHit>,
}

#[derive(Debug, Serialize)]
pub struct ChildSearchResponse {
    pub q: String,
    pub kind: String,
    pub results: ChildSearchResults,
    pub next_page_token: Option<String>,
}

/// `GET /api/search` — child-only, allowlist-bounded search.
///
/// Searches across:
///
/// - **Channels** the child can reach via `allowlisted_channels` or
///   their subscriptions.
/// - **Videos** in `allowlisted_videos`, `watch_history`, and the
///   recent uploads cache for allowlisted channels.
///
/// The query is logged to `search_log` regardless of result count so
/// parents can see what their child is searching for.
pub async fn child_search(
    State(state): State<AppState>,
    current: CurrentAccount,
    Query(q): Query<ChildSearchQuery>,
) -> AppResult<Json<ChildSearchResponse>> {
    // The route is gated by `require_child` middleware in
    // [`crate::routes::router`], so we don't re-check the role here.

    let trimmed = q.q.trim();
    if trimmed.is_empty() {
        return Err(AppError::BadRequest("q is required".into()));
    }
    let kind_label = q.kind.clone().unwrap_or_else(|| "all".to_string());
    let limit = q.limit.unwrap_or(DEFAULT_LIMIT).min(MAX_LIMIT) as i64;
    let pattern = format!("%{}%", trimmed.replace('%', "\\%").replace('_', "\\_"));

    let cursor = q
        .page_token
        .as_deref()
        .and_then(decode_page_token)
        .unwrap_or_default();
    let offset = cursor.offset.max(0);

    let mut results = ChildSearchResults {
        channels: Vec::new(),
        videos: Vec::new(),
    };

    let want_channels = matches!(kind_label.as_str(), "channel" | "all");
    let want_videos = matches!(kind_label.as_str(), "video" | "all");

    if want_channels {
        results.channels = search_channels(&state, current.id, &pattern, limit, offset).await?;
    }
    if want_videos {
        results.videos = search_videos(&state, current.id, &pattern, limit, offset).await?;
    }

    // Apply access control to every video hit so a blocked-then-
    // allowlisted edge case still hides the video. The channel ID is
    // enough to keep the channel-allowlist branch alive.
    let mut filtered_videos = Vec::with_capacity(results.videos.len());
    for hit in results.videos.drain(..) {
        if can_child_view(
            &state.db,
            current.id,
            &hit.video_id,
            hit.channel_id.as_deref(),
        )
        .await
        .unwrap_or(false)
        {
            filtered_videos.push(hit);
        }
    }
    results.videos = filtered_videos;

    let total = results.channels.len() + results.videos.len();

    // Always log, regardless of result count. Only log the first page so
    // a single search session doesn't produce duplicate `search_log`
    // rows on every "load more" request.
    if offset == 0 {
        // Detect "first time we've ever seen this query for this child"
        // *before* writing the new row, so we can dispatch a
        // `new_search_term` notification.
        let prior: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM search_log WHERE child_account_id = ? AND query = ?",
        )
        .bind(current.id)
        .bind(trimmed)
        .fetch_one(&state.db)
        .await
        .unwrap_or(0);

        let _ = sqlx::query(
            "INSERT INTO search_log (child_account_id, query, result_count) VALUES (?, ?, ?)",
        )
        .bind(current.id)
        .bind(trimmed)
        .bind(total as i64)
        .execute(&state.db)
        .await;

        if prior == 0 {
            let display_name: String =
                sqlx::query_scalar("SELECT display_name FROM accounts WHERE id = ?")
                    .bind(current.id)
                    .fetch_one(&state.db)
                    .await
                    .unwrap_or_else(|_| "A child".to_string());
            let _ = crate::services::notifications::dispatch_new_search_term(
                &state.db,
                current.id,
                &display_name,
                trimmed,
            )
            .await;
        }
    }

    // Emit a `next_page_token` only if any individual bucket appears to
    // be saturated at the per-bucket `limit`. A bucket with strictly
    // fewer rows than `limit` has been fully drained.
    let has_more = results.channels.len() as i64 >= limit || results.videos.len() as i64 >= limit;
    let next_page_token = has_more.then(|| {
        encode_page_token(&PageCursor {
            offset: offset + limit,
        })
    });

    Ok(Json(ChildSearchResponse {
        q: trimmed.to_string(),
        kind: kind_label,
        results,
        next_page_token,
    }))
}

async fn search_channels(
    state: &AppState,
    child_id: i64,
    pattern: &str,
    limit: i64,
    offset: i64,
) -> AppResult<Vec<ChildChannelHit>> {
    // Two sources of channel visibility:
    //   1. Direct allowlist
    //   2. The child's subscribed channels (which the child can browse
    //      via /child/channels regardless of the per-video allowlist)
    //
    // We UNION on the (channel_id, channel_title, thumbnail) shape so
    // the result is naturally deduplicated.
    let rows: Vec<(String, String, Option<String>)> = sqlx::query_as(
        "SELECT channel_id, channel_title, channel_thumbnail_url FROM ( \
            SELECT channel_id, channel_title, channel_thumbnail_url \
              FROM allowlisted_channels \
              WHERE child_account_id = ? \
            UNION \
            SELECT channel_id, channel_title, channel_thumbnail_url \
              FROM child_subscriptions \
              WHERE child_account_id = ? AND is_deleted = 0 \
         ) \
         WHERE channel_title LIKE ? ESCAPE '\\' \
         ORDER BY channel_title \
         LIMIT ? OFFSET ?",
    )
    .bind(child_id)
    .bind(child_id)
    .bind(pattern)
    .bind(limit)
    .bind(offset)
    .fetch_all(&state.db)
    .await?;
    Ok(rows
        .into_iter()
        .map(|(id, title, thumb)| ChildChannelHit {
            channel_id: id,
            channel_title: title,
            channel_thumbnail_url: thumb,
        })
        .collect())
}

async fn search_videos(
    state: &AppState,
    child_id: i64,
    pattern: &str,
    limit: i64,
    offset: i64,
) -> AppResult<Vec<ChildVideoHit>> {
    // We search local cached metadata first to keep the request fast.
    // Three local sources, gathered via UNION ALL + a per-video GROUP BY:
    //
    //   1. allowlisted_videos       — direct per-video allowlist
    //   2. watch_history            — videos already proven viewable
    //   3. channel_videos via       — videos surfaced through an
    //      allowlisted_channels       allowlisted channel's archive
    //
    // (3) is necessary because a child whose access derives *only*
    // from a channel allowlist would otherwise be unable to search
    // for those videos at all. `channel_videos` holds the full
    // archive populated by RSS + yt-dlp backfill.
    //
    // We use UNION ALL + GROUP BY rather than UNION because UNION
    // dedups only on full-row equality — buckets 1–2 have no
    // `channel_id` while bucket 3 does. GROUP BY video_id collapses
    // them; MAX(channel_id) prefers the bucket-3 form (real id beats
    // NULL under MAX), giving `can_child_view` the data it needs to
    // exercise the channel-allowlist branch.
    type SearchRow = (
        String,
        String,
        Option<String>,
        Option<String>,
        Option<String>,
    );
    let rows: Vec<SearchRow> = sqlx::query_as(
        "SELECT video_id, \
                MAX(video_title) AS video_title, \
                MAX(channel_id) AS channel_id, \
                MAX(channel_title) AS channel_title, \
                MAX(video_thumbnail_url) AS video_thumbnail_url \
         FROM ( \
            SELECT video_id, video_title, \
                   NULL AS channel_id, \
                   channel_title, video_thumbnail_url \
              FROM allowlisted_videos WHERE child_account_id = ? \
            UNION ALL \
            SELECT video_id, video_title, \
                   NULL AS channel_id, \
                   channel_title, video_thumbnail_url \
              FROM watch_history WHERE child_account_id = ? \
            UNION ALL \
            SELECT cv.video_id, cv.title AS video_title, \
                   cv.channel_id, cv.channel_title, \
                   cv.thumbnail_url AS video_thumbnail_url \
              FROM channel_videos cv \
              INNER JOIN allowlisted_channels ac \
                ON ac.channel_id = cv.channel_id \
              WHERE ac.child_account_id = ? AND cv.is_deleted = 0 \
         ) \
         WHERE video_title LIKE ? ESCAPE '\\' \
         GROUP BY video_id \
         ORDER BY video_title \
         LIMIT ? OFFSET ?",
    )
    .bind(child_id)
    .bind(child_id)
    .bind(child_id)
    .bind(pattern)
    .bind(limit)
    .bind(offset)
    .fetch_all(&state.db)
    .await
    .unwrap_or_default();

    Ok(rows
        .into_iter()
        .map(
            |(video_id, title, channel_id, ch_title, thumb)| ChildVideoHit {
                video_id,
                title,
                // Surfaced for `can_child_view`'s channel-allowlist branch
                // in the layer-B post-filter (see `child_search`).
                channel_id,
                channel_title: ch_title,
                thumbnail_url: thumb,
            },
        )
        .collect())
}

/// Helper: pick the highest-resolution thumbnail URL from a YouTube
/// `thumbnails` map. Currently unused locally but exposed for future
/// suggestion endpoints.
#[allow(dead_code)]
pub fn pick_thumb_url(thumbs: &std::collections::HashMap<String, ThumbnailInfo>) -> Option<String> {
    for key in ["maxres", "high", "standard", "medium", "default"] {
        if let Some(t) = thumbs.get(key) {
            return Some(t.url.clone());
        }
    }
    None
}
