//! Child home-page feed routes.
//!
//! Two read-only endpoints back the child home page:
//!
//! - `GET /api/feed/continue-watching` — recently-watched items from
//!   `watch_history`, filtered through access control to drop videos
//!   the parent has since revoked.
//! - `GET /api/feed/new-videos` — fresh uploads from each allowlisted
//!   channel + each allowlisted playlist, deduped + sorted.

use axum::{extract::State, Json};
use serde::Serialize;

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
        match yt.list_channel_videos(channel_id, PER_SOURCE_LIMIT, None).await {
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
        match yt.list_playlist_items(playlist_id, PER_SOURCE_LIMIT, None).await {
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
