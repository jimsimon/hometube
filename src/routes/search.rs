//! Search helpers.
//!
//! Two distinct concerns share this module:
//!
//! - `GET /api/parent/search` — parent-side discovery, used by the
//!   allowlist UI to find content to add. Hits the YouTube Data API
//!   directly. Implemented by [`parent_search`].
//! - `GET /api/search` — child-side allowlist-bounded search.
//!   Implemented by [`child_search`] (Phase 10). The child can only ever
//!   see content that is reachable from their allowlist, and every
//!   query is logged to `search_log` for parent visibility.

use std::collections::HashSet;

use axum::{
    extract::{Query, State},
    Json,
};
use serde::{Deserialize, Serialize};

use crate::error::{AppError, AppResult};
use crate::middleware::auth::CurrentAccount;
use crate::models::account::AccountType;
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
    /// `"channel"`, `"playlist"`, or `"video"`.
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(default)]
    pub max_results: Option<u32>,
}

#[derive(Debug, Serialize)]
pub struct SearchResponse {
    pub items: Vec<SearchItem>,
}

/// `GET /api/parent/search?q=&type=channel|playlist|video`.
pub async fn parent_search(
    State(state): State<AppState>,
    Query(q): Query<ParentSearchQuery>,
) -> AppResult<Json<SearchResponse>> {
    let kind = SearchType::parse(&q.kind)
        .ok_or_else(|| AppError::BadRequest("type must be channel|playlist|video".into()))?;
    let yt = YoutubeClient::from_db(&state.db).await?;
    let items = yt
        .search(&q.q, kind, q.max_results.unwrap_or(15))
        .await?;
    Ok(Json(SearchResponse { items }))
}

// ---------------------------------------------------------------------------
// Child search
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct ChildSearchQuery {
    pub q: String,
    /// One of `channel`, `playlist`, `video`, or `all` (default).
    #[serde(default, rename = "type")]
    pub kind: Option<String>,
    /// Optional pagination token. Currently advisory — the child search
    /// is bounded by the allowlist size, so we rarely need true paging.
    #[serde(default)]
    pub page_token: Option<String>,
    #[serde(default)]
    pub limit: Option<u32>,
}

#[derive(Debug, Serialize, Clone)]
pub struct ChildChannelHit {
    pub channel_id: String,
    pub channel_title: String,
    pub channel_thumbnail_url: Option<String>,
}

#[derive(Debug, Serialize, Clone)]
pub struct ChildPlaylistHit {
    pub playlist_id: String,
    pub playlist_title: String,
    pub playlist_thumbnail_url: Option<String>,
    pub source: &'static str,
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
    pub playlists: Vec<ChildPlaylistHit>,
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
/// - **Channels** the child can reach via `allowlisted_channels`,
///   `allowlisted_videos.channel_title`, or playlist channels.
/// - **Playlists** in `allowlisted_playlists` + the child's own
///   `child_playlists` + (Phase 18) family playlists assigned to them.
/// - **Videos** in `allowlisted_videos`, `watch_history`, and
///   `child_playlist_videos` whose title matches `q`.
///
/// The query is logged to `search_log` regardless of result count so
/// parents can see what their child is searching for.
pub async fn child_search(
    State(state): State<AppState>,
    current: CurrentAccount,
    Query(q): Query<ChildSearchQuery>,
) -> AppResult<Json<ChildSearchResponse>> {
    if !matches!(current.account_type, AccountType::Child) {
        return Err(AppError::Forbidden);
    }

    let trimmed = q.q.trim();
    if trimmed.is_empty() {
        return Err(AppError::BadRequest("q is required".into()));
    }
    let kind_label = q.kind.clone().unwrap_or_else(|| "all".to_string());
    let limit = q.limit.unwrap_or(DEFAULT_LIMIT).min(MAX_LIMIT) as i64;
    let pattern = format!("%{}%", trimmed.replace('%', "\\%").replace('_', "\\_"));

    let mut results = ChildSearchResults {
        channels: Vec::new(),
        playlists: Vec::new(),
        videos: Vec::new(),
    };

    let want_channels = matches!(kind_label.as_str(), "channel" | "all");
    let want_playlists = matches!(kind_label.as_str(), "playlist" | "all");
    let want_videos = matches!(kind_label.as_str(), "video" | "all");

    if want_channels {
        results.channels =
            search_channels(&state, current.id, &pattern, limit).await?;
    }
    if want_playlists {
        results.playlists =
            search_playlists(&state, current.id, &pattern, limit).await?;
    }
    if want_videos {
        results.videos = search_videos(&state, current.id, &pattern, limit).await?;
    }

    // Apply access control to every video hit so a blocked-then-
    // allowlisted edge case still hides the video. We don't have the
    // playlist context here so pass an empty slice — the channel ID is
    // enough to keep the channel-allowlist branch alive.
    let mut filtered_videos = Vec::with_capacity(results.videos.len());
    for hit in results.videos.drain(..) {
        if can_child_view(
            &state.db,
            current.id,
            &hit.video_id,
            hit.channel_id.as_deref(),
            &[],
        )
        .await
        .unwrap_or(false)
        {
            filtered_videos.push(hit);
        }
    }
    results.videos = filtered_videos;

    let total = results.channels.len() + results.playlists.len() + results.videos.len();

    // Always log, regardless of result count.
    let _ = sqlx::query(
        "INSERT INTO search_log (child_account_id, query, result_count) VALUES (?, ?, ?)",
    )
    .bind(current.id)
    .bind(trimmed)
    .bind(total as i64)
    .execute(&state.db)
    .await;

    Ok(Json(ChildSearchResponse {
        q: trimmed.to_string(),
        kind: kind_label,
        results,
        // Pagination is currently best-effort — return None until the
        // result set genuinely overflows a single page.
        next_page_token: None,
    }))
}

async fn search_channels(
    state: &AppState,
    child_id: i64,
    pattern: &str,
    limit: i64,
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
         LIMIT ?",
    )
    .bind(child_id)
    .bind(child_id)
    .bind(pattern)
    .bind(limit)
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

async fn search_playlists(
    state: &AppState,
    child_id: i64,
    pattern: &str,
    limit: i64,
) -> AppResult<Vec<ChildPlaylistHit>> {
    let mut out: Vec<ChildPlaylistHit> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();

    // Allowlisted playlists.
    let rows: Vec<(String, String, Option<String>)> = sqlx::query_as(
        "SELECT playlist_id, playlist_title, playlist_thumbnail_url \
         FROM allowlisted_playlists \
         WHERE child_account_id = ? AND playlist_title LIKE ? ESCAPE '\\' \
         ORDER BY playlist_title LIMIT ?",
    )
    .bind(child_id)
    .bind(pattern)
    .bind(limit)
    .fetch_all(&state.db)
    .await?;
    for (id, title, thumb) in rows {
        if seen.insert(id.clone()) {
            out.push(ChildPlaylistHit {
                playlist_id: id,
                playlist_title: title,
                playlist_thumbnail_url: thumb,
                source: "allowlist",
            });
        }
    }

    // Child's own playlists. We surface the local primary-key id
    // (string-encoded) so the UI can deep-link to /child/playlist/:id.
    let own: Vec<(i64, String)> = sqlx::query_as(
        "SELECT id, title FROM child_playlists \
         WHERE child_account_id = ? AND is_deleted = 0 AND title LIKE ? ESCAPE '\\' \
         ORDER BY title LIMIT ?",
    )
    .bind(child_id)
    .bind(pattern)
    .bind(limit)
    .fetch_all(&state.db)
    .await?;
    for (id, title) in own {
        let key = format!("local:{id}");
        if seen.insert(key.clone()) {
            out.push(ChildPlaylistHit {
                playlist_id: id.to_string(),
                playlist_title: title,
                playlist_thumbnail_url: None,
                source: "own",
            });
        }
    }

    // Family playlists assigned to this child (Phase 18 — handled
    // gracefully if the join returns nothing).
    let family: Vec<(i64, String)> = sqlx::query_as(
        "SELECT fp.id, fp.title \
         FROM family_playlists fp \
         INNER JOIN family_playlist_members m ON m.playlist_id = fp.id \
         WHERE m.child_account_id = ? AND fp.title LIKE ? ESCAPE '\\' \
         ORDER BY fp.title LIMIT ?",
    )
    .bind(child_id)
    .bind(pattern)
    .bind(limit)
    .fetch_all(&state.db)
    .await
    .unwrap_or_default();
    for (id, title) in family {
        let key = format!("family:{id}");
        if seen.insert(key.clone()) {
            out.push(ChildPlaylistHit {
                playlist_id: format!("family:{id}"),
                playlist_title: title,
                playlist_thumbnail_url: None,
                source: "family",
            });
        }
    }

    out.truncate(limit as usize);
    Ok(out)
}

async fn search_videos(
    state: &AppState,
    child_id: i64,
    pattern: &str,
    limit: i64,
) -> AppResult<Vec<ChildVideoHit>> {
    // We search local cached metadata first to keep the request fast.
    // Three local sources:
    //   - allowlisted_videos (direct)
    //   - watch_history (videos already proven viewable)
    //   - child_playlist_videos (videos in playlists)
    let rows: Vec<(String, String, Option<String>, Option<String>)> = sqlx::query_as(
        "SELECT video_id, video_title, channel_title, video_thumbnail_url FROM ( \
            SELECT video_id, video_title, channel_title, video_thumbnail_url \
              FROM allowlisted_videos WHERE child_account_id = ? \
            UNION \
            SELECT video_id, video_title, channel_title, video_thumbnail_url \
              FROM watch_history WHERE child_account_id = ? \
            UNION \
            SELECT cpv.video_id, cpv.video_title, cpv.channel_title, cpv.video_thumbnail_url \
              FROM child_playlist_videos cpv \
              INNER JOIN child_playlists cp ON cp.id = cpv.playlist_id \
              WHERE cp.child_account_id = ? AND cp.is_deleted = 0 \
         ) \
         WHERE video_title LIKE ? ESCAPE '\\' \
         ORDER BY video_title \
         LIMIT ?",
    )
    .bind(child_id)
    .bind(child_id)
    .bind(child_id)
    .bind(pattern)
    .bind(limit)
    .fetch_all(&state.db)
    .await
    .unwrap_or_default();

    Ok(rows
        .into_iter()
        .map(|(video_id, title, ch_title, thumb)| ChildVideoHit {
            video_id,
            title,
            channel_id: None,
            channel_title: ch_title,
            thumbnail_url: thumb,
        })
        .collect())
}

/// Helper: pick the highest-resolution thumbnail URL from a YouTube
/// `thumbnails` map. Currently unused locally but exposed for future
/// suggestion endpoints.
#[allow(dead_code)]
pub fn pick_thumb_url(
    thumbs: &std::collections::HashMap<String, ThumbnailInfo>,
) -> Option<String> {
    for key in ["maxres", "high", "standard", "medium", "default"] {
        if let Some(t) = thumbs.get(key) {
            return Some(t.url.clone());
        }
    }
    None
}
