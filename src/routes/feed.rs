//! Child home-page feed routes.
//!
//! Three read-only endpoints back the child UI:
//!
//! - `GET /api/feed/continue-watching` — recently-watched items from
//!   `watch_history`, filtered through access control to drop videos
//!   the parent has since revoked.
//! - `GET /api/feed/new-videos` — fresh uploads from each allowlisted
//!   channel + each allowlisted playlist, deduped + sorted.
//! - `GET /api/feed/up-next` — the next videos to play after the one
//!   currently on screen, given a `from=playlist:ID|channel:ID|video:ID`
//!   context. Drops blocked + access-denied items.

use axum::{
    extract::{Query, State},
    Json,
};
use serde::{Deserialize, Serialize};

use crate::error::AppResult;
use crate::middleware::auth::CurrentAccount;
use crate::services::access::{
    can_child_view, child_allowlisted_channel_ids, child_allowlisted_playlist_ids,
};
use crate::services::youtube::YoutubeClient;
use crate::state::AppState;

/// Default limit for "continue watching".
const CONTINUE_LIMIT: i64 = 20;
/// Hard cap on the new-videos feed.
const NEW_VIDEOS_LIMIT: usize = 30;
/// Per-channel/playlist sample size when building the new-videos feed.
const PER_SOURCE_LIMIT: u32 = 5;

#[derive(Debug, Serialize, sqlx::FromRow, Clone)]
pub struct ContinueWatchingItem {
    pub video_id: String,
    pub video_title: String,
    pub video_thumbnail_url: Option<String>,
    pub channel_title: Option<String>,
    pub duration_seconds: Option<i64>,
    pub progress_seconds: i64,
    pub last_watched_at: i64,
}

/// `GET /api/feed/continue-watching`.
pub async fn continue_watching(
    State(state): State<AppState>,
    current: CurrentAccount,
) -> AppResult<Json<Vec<ContinueWatchingItem>>> {
    let rows: Vec<ContinueWatchingItem> = sqlx::query_as(
        "SELECT video_id, video_title, video_thumbnail_url, channel_title, \
                duration_seconds, progress_seconds, last_watched_at \
         FROM watch_history \
         WHERE child_account_id = ? \
         ORDER BY last_watched_at DESC \
         LIMIT ?",
    )
    .bind(current.id)
    .bind(CONTINUE_LIMIT)
    .fetch_all(&state.db)
    .await?;

    // Filter through access control. We don't know the channel/playlist
    // for historical entries, so we just check the basic allowlist
    // tables (which is what `can_child_view` does).
    let mut filtered = Vec::with_capacity(rows.len());
    for row in rows {
        if can_child_view(&state.db, current.id, &row.video_id, None, &[])
            .await
            .unwrap_or(false)
        {
            filtered.push(row);
        }
    }
    Ok(Json(filtered))
}

#[derive(Debug, Serialize, Clone)]
pub struct NewVideoItem {
    pub video_id: String,
    pub title: String,
    pub channel_id: Option<String>,
    pub channel_title: Option<String>,
    pub thumbnail_url: Option<String>,
    pub published_at: Option<String>,
    pub source_kind: String, // "channel" or "playlist"
    pub source_id: String,
}

/// `GET /api/feed/new-videos`.
pub async fn new_videos(
    State(state): State<AppState>,
    current: CurrentAccount,
) -> AppResult<Json<Vec<NewVideoItem>>> {
    let yt = match YoutubeClient::from_db(&state.db).await {
        Ok(y) => y,
        Err(_) => {
            // No API key configured — return empty list.
            return Ok(Json(Vec::new()));
        }
    };

    let channels = child_allowlisted_channel_ids(&state.db, current.id).await?;
    let playlists = child_allowlisted_playlist_ids(&state.db, current.id).await?;

    let mut items: Vec<NewVideoItem> = Vec::new();

    for channel_id in &channels {
        match yt
            .list_channel_videos(channel_id, PER_SOURCE_LIMIT, None)
            .await
        {
            Ok(page) => {
                for it in page.items {
                    items.push(NewVideoItem {
                        video_id: it.video_id,
                        title: it.title,
                        channel_id: it.channel_id,
                        channel_title: it.channel_title,
                        thumbnail_url: pick_thumbnail(&it.thumbnails),
                        published_at: it.published_at,
                        source_kind: "channel".into(),
                        source_id: channel_id.clone(),
                    });
                }
            }
            Err(err) => tracing::warn!(%channel_id, %err, "failed to list channel videos"),
        }
    }
    for playlist_id in &playlists {
        match yt
            .list_playlist_items(playlist_id, PER_SOURCE_LIMIT, None)
            .await
        {
            Ok(page) => {
                for it in page.items {
                    items.push(NewVideoItem {
                        video_id: it.video_id,
                        title: it.title,
                        channel_id: it.channel_id,
                        channel_title: it.channel_title,
                        thumbnail_url: pick_thumbnail(&it.thumbnails),
                        published_at: it.published_at,
                        source_kind: "playlist".into(),
                        source_id: playlist_id.clone(),
                    });
                }
            }
            Err(err) => tracing::warn!(%playlist_id, %err, "failed to list playlist items"),
        }
    }

    // Dedupe by video_id, preferring the latest published_at.
    items.sort_by(|a, b| b.published_at.cmp(&a.published_at));
    let mut seen = std::collections::HashSet::new();
    items.retain(|it| seen.insert(it.video_id.clone()));
    items.truncate(NEW_VIDEOS_LIMIT);

    // Drop videos the parent has since blocked.
    let mut filtered = Vec::with_capacity(items.len());
    for it in items {
        if can_child_view(
            &state.db,
            current.id,
            &it.video_id,
            it.channel_id.as_deref(),
            std::slice::from_ref(&it.source_id),
        )
        .await
        .unwrap_or(false)
        {
            filtered.push(it);
        }
    }
    Ok(Json(filtered))
}

fn pick_thumbnail(
    thumbs: &std::collections::HashMap<String, crate::services::youtube::ThumbnailInfo>,
) -> Option<String> {
    for key in ["maxres", "high", "standard", "medium", "default"] {
        if let Some(t) = thumbs.get(key) {
            return Some(t.url.clone());
        }
    }
    None
}

// ---------------------------------------------------------------------------
// Up-next queue
// ---------------------------------------------------------------------------

/// Default queue length for `/api/feed/up-next`.
const UP_NEXT_DEFAULT_LIMIT: usize = 10;

#[derive(Debug, Deserialize)]
pub struct UpNextQuery {
    /// Source context. Recognised values:
    ///
    /// - `playlist:<id>` — videos from a `child_playlists` row (own or
    ///   library) ordered by `position`.
    /// - `channel:<id>` — channel uploads, drawn from the YouTube API.
    /// - `video:<id>` — fall back to "more from the same channel" or
    ///   the new-videos feed if no channel context is available.
    /// - missing — returns the new-videos feed.
    #[serde(default)]
    pub from: Option<String>,
    /// Currently-playing video ID. Excluded from the result.
    #[serde(default)]
    pub current_video: Option<String>,
    #[serde(default)]
    pub limit: Option<usize>,
}

#[derive(Debug, Serialize, Clone)]
pub struct UpNextItem {
    pub video_id: String,
    pub title: String,
    pub channel_id: Option<String>,
    pub channel_title: Option<String>,
    pub thumbnail_url: Option<String>,
}

/// `GET /api/feed/up-next`.
pub async fn up_next(
    State(state): State<AppState>,
    current: CurrentAccount,
    Query(q): Query<UpNextQuery>,
) -> AppResult<Json<Vec<UpNextItem>>> {
    let limit = q.limit.unwrap_or(UP_NEXT_DEFAULT_LIMIT).clamp(1, 50);

    // Parse `from`.
    let (kind, id) = parse_from(q.from.as_deref());

    let raw: Vec<UpNextItem> = match (kind, id) {
        (Some("playlist"), Some(playlist_id)) => {
            up_next_from_playlist(&state, current.id, playlist_id).await?
        }
        (Some("channel"), Some(channel_id)) => up_next_from_channel(&state, channel_id).await?,
        _ => up_next_from_new_videos(&state, current.id).await?,
    };

    // Drop the current video and access-control failures.
    let mut out = Vec::with_capacity(limit);
    for item in raw {
        if Some(&item.video_id) == q.current_video.as_ref() {
            continue;
        }
        if can_child_view(
            &state.db,
            current.id,
            &item.video_id,
            item.channel_id.as_deref(),
            &[],
        )
        .await
        .unwrap_or(false)
        {
            out.push(item);
            if out.len() >= limit {
                break;
            }
        }
    }
    Ok(Json(out))
}

fn parse_from(from: Option<&str>) -> (Option<&str>, Option<&str>) {
    let Some(s) = from else {
        return (None, None);
    };
    let mut iter = s.splitn(2, ':');
    let kind = iter.next();
    let id = iter.next();
    (kind, id)
}

async fn up_next_from_playlist(
    state: &AppState,
    child_id: i64,
    playlist_id: &str,
) -> AppResult<Vec<UpNextItem>> {
    // Match either by primary-key id or by youtube_playlist_id.
    let row: Option<(i64,)> = sqlx::query_as(
        "SELECT id FROM child_playlists \
         WHERE child_account_id = ? AND is_deleted = 0 \
           AND (CAST(id AS TEXT) = ? OR youtube_playlist_id = ?)",
    )
    .bind(child_id)
    .bind(playlist_id)
    .bind(playlist_id)
    .fetch_optional(&state.db)
    .await?;
    let Some((local_id,)) = row else {
        return Ok(Vec::new());
    };

    let rows: Vec<(String, String, Option<String>, Option<String>)> = sqlx::query_as(
        "SELECT video_id, video_title, video_thumbnail_url, channel_title \
         FROM child_playlist_videos WHERE playlist_id = ? ORDER BY position",
    )
    .bind(local_id)
    .fetch_all(&state.db)
    .await?;
    Ok(rows
        .into_iter()
        .map(|(video_id, title, thumb, ch)| UpNextItem {
            video_id,
            title,
            channel_id: None,
            channel_title: ch,
            thumbnail_url: thumb,
        })
        .collect())
}

async fn up_next_from_channel(state: &AppState, channel_id: &str) -> AppResult<Vec<UpNextItem>> {
    let yt = match crate::services::youtube::YoutubeClient::from_db(&state.db).await {
        Ok(y) => y,
        Err(_) => return Ok(Vec::new()),
    };
    let page = yt
        .list_channel_videos(channel_id, 25, None)
        .await
        .unwrap_or_else(|err| {
            tracing::warn!(%err, "list_channel_videos failed");
            crate::services::youtube::Page {
                items: Vec::new(),
                next_page_token: None,
            }
        });
    Ok(page
        .items
        .into_iter()
        .map(|it| UpNextItem {
            video_id: it.video_id,
            title: it.title,
            channel_id: it.channel_id.or_else(|| Some(channel_id.to_string())),
            channel_title: it.channel_title,
            thumbnail_url: pick_thumbnail(&it.thumbnails),
        })
        .collect())
}

async fn up_next_from_new_videos(state: &AppState, child_id: i64) -> AppResult<Vec<UpNextItem>> {
    let yt = match crate::services::youtube::YoutubeClient::from_db(&state.db).await {
        Ok(y) => y,
        Err(_) => return Ok(Vec::new()),
    };

    let channels = child_allowlisted_channel_ids(&state.db, child_id).await?;
    let playlists = child_allowlisted_playlist_ids(&state.db, child_id).await?;
    let mut out: Vec<UpNextItem> = Vec::new();
    for channel_id in &channels {
        if let Ok(page) = yt.list_channel_videos(channel_id, 5, None).await {
            for it in page.items {
                out.push(UpNextItem {
                    video_id: it.video_id,
                    title: it.title,
                    channel_id: it.channel_id.or_else(|| Some(channel_id.clone())),
                    channel_title: it.channel_title,
                    thumbnail_url: pick_thumbnail(&it.thumbnails),
                });
            }
        }
    }
    for playlist_id in &playlists {
        if let Ok(page) = yt.list_playlist_items(playlist_id, 5, None).await {
            for it in page.items {
                out.push(UpNextItem {
                    video_id: it.video_id,
                    title: it.title,
                    channel_id: it.channel_id,
                    channel_title: it.channel_title,
                    thumbnail_url: pick_thumbnail(&it.thumbnails),
                });
            }
        }
    }
    Ok(out)
}
