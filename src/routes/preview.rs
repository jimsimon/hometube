//! Parental preview routes (parent-only).
//!
//! These endpoints mirror their child-side counterparts but bypass the
//! per-child allowlist check, so a parent can inspect a video, channel,
//! or playlist before deciding to add it to a child's allowlist.
//!
//! Routes:
//! - `GET /api/preview/video/:videoId`
//! - `GET /api/preview/channel/:channelId`
//! - `GET /api/preview/playlist/:playlistId`
//!
//! All three are gated by [`crate::middleware::account_type::require_parent`].

use axum::{
    extract::{Path, State},
    Json,
};
use serde::Serialize;

use crate::error::AppResult;
use crate::routes::videos::VideoMetadata;
use crate::services::video_cache::VideoCache;
use crate::services::youtube::{ChannelInfo, PlaylistInfo, PlaylistItem, YoutubeClient};
use crate::state::AppState;

/// `GET /api/preview/video/:videoId` — yt-dlp metadata, allowlist
/// bypassed.
pub async fn preview_video(
    State(state): State<AppState>,
    Path(video_id): Path<String>,
) -> AppResult<Json<VideoMetadata>> {
    let cache = VideoCache::new();
    let result = cache
        .get_or_extract(&state.db, &state.config, &video_id)
        .await?;
    let thumb = pick_thumb(&result);
    Ok(Json(VideoMetadata {
        id: result.id.clone(),
        title: result.title.clone(),
        channel_id: result.channel_id.clone(),
        channel_title: result.channel_title.clone(),
        duration_seconds: result.duration,
        thumbnail_url: thumb,
    }))
}

fn pick_thumb(result: &crate::services::ytdlp::ExtractResult) -> Option<String> {
    if let Some(direct) = result.thumbnail.clone() {
        return Some(direct);
    }
    result
        .thumbnails
        .iter()
        .max_by_key(|t| t.width.unwrap_or(0))
        .map(|t| t.url.clone())
}

#[derive(Debug, Serialize)]
pub struct ChannelPreview {
    #[serde(flatten)]
    pub channel: ChannelInfo,
    pub videos: Vec<PlaylistItem>,
    pub next_page_token: Option<String>,
}

/// `GET /api/preview/channel/:channelId` — channel info + recent uploads.
pub async fn preview_channel(
    State(state): State<AppState>,
    Path(channel_id): Path<String>,
) -> AppResult<Json<ChannelPreview>> {
    let yt = YoutubeClient::from_db(&state.db).await?;
    let channel = yt
        .get_channel(&channel_id)
        .await?
        .ok_or(crate::error::AppError::NotFound)?;
    let page = yt.list_channel_videos(&channel_id, 24, None).await?;
    Ok(Json(ChannelPreview {
        channel,
        videos: page.items,
        next_page_token: page.next_page_token,
    }))
}

#[derive(Debug, Serialize)]
pub struct PlaylistPreview {
    #[serde(flatten)]
    pub playlist: PlaylistInfo,
    pub videos: Vec<PlaylistItem>,
    pub next_page_token: Option<String>,
}

/// `GET /api/preview/playlist/:playlistId` — playlist info + items.
pub async fn preview_playlist(
    State(state): State<AppState>,
    Path(playlist_id): Path<String>,
) -> AppResult<Json<PlaylistPreview>> {
    let yt = YoutubeClient::from_db(&state.db).await?;
    let playlist = yt
        .get_playlist(&playlist_id)
        .await?
        .ok_or(crate::error::AppError::NotFound)?;
    let page = yt.list_playlist_items(&playlist_id, 50, None).await?;
    Ok(Json(PlaylistPreview {
        playlist,
        videos: page.items,
        next_page_token: page.next_page_token,
    }))
}
