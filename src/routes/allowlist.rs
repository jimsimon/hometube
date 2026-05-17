//! Allowlist management routes (parent only).
//!
//! Three flavours: channels, playlists, and individual videos. Each
//! follows the same shape:
//!
//! - `GET    /api/children/:id/allowlist/{kind}`
//! - `POST   /api/children/:id/allowlist/{kind}`           (body: `{ channel_id|playlist_id|video_id }`)
//! - `DELETE /api/children/:id/allowlist/{kind}/:itemId`
//!
//! The `:id` path parameter must refer to a *child* account; parent IDs
//! are rejected with `400 Bad Request`. Metadata (title, thumbnail) is
//! fetched from YouTube (via the discovery sidecar) at insert time so
//! the UI doesn't have to re-resolve names every time it lists the
//! allowlist.

use axum::{
    extract::{Path, State},
    http::StatusCode,
    Json,
};
use serde::{Deserialize, Serialize};

use crate::error::{AppError, AppResult};
use crate::middleware::auth::CurrentAccount;
use crate::services::access;
use crate::services::youtube::YoutubeClient;
use crate::state::AppState;

// ---------------------------------------------------------------------------
// Channels
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize, sqlx::FromRow)]
pub struct AllowlistedChannel {
    pub id: i64,
    pub channel_id: String,
    pub channel_title: String,
    pub channel_thumbnail_url: Option<String>,
    pub created_at: i64,
}

#[derive(Debug, Deserialize)]
pub struct AddChannelBody {
    pub channel_id: String,
}

/// `GET /api/children/:id/allowlist/channels`.
pub async fn list_channels(
    State(state): State<AppState>,
    Path(child_id): Path<i64>,
) -> AppResult<Json<Vec<AllowlistedChannel>>> {
    require_child_id(&state, child_id).await?;
    let rows: Vec<AllowlistedChannel> = sqlx::query_as(
        "SELECT id, channel_id, channel_title, channel_thumbnail_url, created_at \
         FROM allowlisted_channels WHERE child_account_id = ? ORDER BY created_at DESC",
    )
    .bind(child_id)
    .fetch_all(&state.db)
    .await?;
    Ok(Json(rows))
}

/// `POST /api/children/:id/allowlist/channels`.
pub async fn add_channel(
    State(state): State<AppState>,
    current: CurrentAccount,
    Path(child_id): Path<i64>,
    Json(body): Json<AddChannelBody>,
) -> AppResult<Json<AllowlistedChannel>> {
    require_child_id(&state, child_id).await?;
    let yt = YoutubeClient::from_db(&state.db).await?;
    let info = yt
        .get_channel(&body.channel_id)
        .await?
        .ok_or_else(|| AppError::BadRequest("channel not found on YouTube".into()))?;
    let thumb = preferred_thumbnail(&info.thumbnails);

    let row: AllowlistedChannel = sqlx::query_as(
        "INSERT INTO allowlisted_channels \
            (child_account_id, channel_id, channel_title, channel_thumbnail_url, added_by) \
         VALUES (?, ?, ?, ?, ?) \
         ON CONFLICT(child_account_id, channel_id) DO UPDATE SET \
            channel_title = excluded.channel_title, \
            channel_thumbnail_url = excluded.channel_thumbnail_url \
         RETURNING id, channel_id, channel_title, channel_thumbnail_url, created_at",
    )
    .bind(child_id)
    .bind(&info.id)
    .bind(&info.title)
    .bind(thumb)
    .bind(current.id)
    .fetch_one(&state.db)
    .await?;
    Ok(Json(row))
}

/// `DELETE /api/children/:id/allowlist/channels/:channelId`.
pub async fn delete_channel(
    State(state): State<AppState>,
    Path((child_id, channel_id)): Path<(i64, String)>,
) -> AppResult<StatusCode> {
    require_child_id(&state, child_id).await?;
    sqlx::query("DELETE FROM allowlisted_channels WHERE child_account_id = ? AND channel_id = ?")
        .bind(child_id)
        .bind(channel_id)
        .execute(&state.db)
        .await?;
    Ok(StatusCode::NO_CONTENT)
}

// ---------------------------------------------------------------------------
// Playlists
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize, sqlx::FromRow)]
pub struct AllowlistedPlaylist {
    pub id: i64,
    pub playlist_id: String,
    pub playlist_title: String,
    pub playlist_thumbnail_url: Option<String>,
    pub created_at: i64,
}

#[derive(Debug, Deserialize)]
pub struct AddPlaylistBody {
    pub playlist_id: String,
}

/// `GET /api/children/:id/allowlist/playlists`.
pub async fn list_playlists(
    State(state): State<AppState>,
    Path(child_id): Path<i64>,
) -> AppResult<Json<Vec<AllowlistedPlaylist>>> {
    require_child_id(&state, child_id).await?;
    let rows: Vec<AllowlistedPlaylist> = sqlx::query_as(
        "SELECT id, playlist_id, playlist_title, playlist_thumbnail_url, created_at \
         FROM allowlisted_playlists WHERE child_account_id = ? ORDER BY created_at DESC",
    )
    .bind(child_id)
    .fetch_all(&state.db)
    .await?;
    Ok(Json(rows))
}

/// `POST /api/children/:id/allowlist/playlists`.
pub async fn add_playlist(
    State(state): State<AppState>,
    current: CurrentAccount,
    Path(child_id): Path<i64>,
    Json(body): Json<AddPlaylistBody>,
) -> AppResult<Json<AllowlistedPlaylist>> {
    require_child_id(&state, child_id).await?;
    let yt = YoutubeClient::from_db(&state.db).await?;
    let info = yt
        .get_playlist(&body.playlist_id)
        .await?
        .ok_or_else(|| AppError::BadRequest("playlist not found on YouTube".into()))?;
    let thumb = preferred_thumbnail(&info.thumbnails);

    let row: AllowlistedPlaylist = sqlx::query_as(
        "INSERT INTO allowlisted_playlists \
            (child_account_id, playlist_id, playlist_title, playlist_thumbnail_url, added_by) \
         VALUES (?, ?, ?, ?, ?) \
         ON CONFLICT(child_account_id, playlist_id) DO UPDATE SET \
            playlist_title = excluded.playlist_title, \
            playlist_thumbnail_url = excluded.playlist_thumbnail_url \
         RETURNING id, playlist_id, playlist_title, playlist_thumbnail_url, created_at",
    )
    .bind(child_id)
    .bind(&info.id)
    .bind(&info.title)
    .bind(thumb)
    .bind(current.id)
    .fetch_one(&state.db)
    .await?;
    Ok(Json(row))
}

/// `DELETE /api/children/:id/allowlist/playlists/:playlistId`.
pub async fn delete_playlist(
    State(state): State<AppState>,
    Path((child_id, playlist_id)): Path<(i64, String)>,
) -> AppResult<StatusCode> {
    require_child_id(&state, child_id).await?;
    sqlx::query("DELETE FROM allowlisted_playlists WHERE child_account_id = ? AND playlist_id = ?")
        .bind(child_id)
        .bind(playlist_id)
        .execute(&state.db)
        .await?;
    Ok(StatusCode::NO_CONTENT)
}

// ---------------------------------------------------------------------------
// Videos
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize, sqlx::FromRow)]
pub struct AllowlistedVideo {
    pub id: i64,
    pub video_id: String,
    pub video_title: String,
    pub video_thumbnail_url: Option<String>,
    pub channel_title: Option<String>,
    pub created_at: i64,
}

#[derive(Debug, Deserialize)]
pub struct AddVideoBody {
    pub video_id: String,
}

/// `GET /api/children/:id/allowlist/videos`.
pub async fn list_videos(
    State(state): State<AppState>,
    Path(child_id): Path<i64>,
) -> AppResult<Json<Vec<AllowlistedVideo>>> {
    require_child_id(&state, child_id).await?;
    let rows: Vec<AllowlistedVideo> = sqlx::query_as(
        "SELECT id, video_id, video_title, video_thumbnail_url, channel_title, created_at \
         FROM allowlisted_videos WHERE child_account_id = ? ORDER BY created_at DESC",
    )
    .bind(child_id)
    .fetch_all(&state.db)
    .await?;
    Ok(Json(rows))
}

/// `POST /api/children/:id/allowlist/videos`.
pub async fn add_video(
    State(state): State<AppState>,
    current: CurrentAccount,
    Path(child_id): Path<i64>,
    Json(body): Json<AddVideoBody>,
) -> AppResult<Json<AllowlistedVideo>> {
    require_child_id(&state, child_id).await?;
    let video_id = parse_video_id(&body.video_id);
    let yt = YoutubeClient::from_db(&state.db).await?;
    let info = yt
        .get_video(&video_id)
        .await?
        .ok_or_else(|| AppError::BadRequest("video not found on YouTube".into()))?;
    let thumb = preferred_thumbnail(&info.thumbnails);

    let row: AllowlistedVideo = sqlx::query_as(
        "INSERT INTO allowlisted_videos \
            (child_account_id, video_id, video_title, video_thumbnail_url, channel_title, added_by) \
         VALUES (?, ?, ?, ?, ?, ?) \
         ON CONFLICT(child_account_id, video_id) DO UPDATE SET \
            video_title = excluded.video_title, \
            video_thumbnail_url = excluded.video_thumbnail_url, \
            channel_title = excluded.channel_title \
         RETURNING id, video_id, video_title, video_thumbnail_url, channel_title, created_at",
    )
    .bind(child_id)
    .bind(&info.id)
    .bind(&info.title)
    .bind(thumb)
    .bind(info.channel_title.clone())
    .bind(current.id)
    .fetch_one(&state.db)
    .await?;
    Ok(Json(row))
}

/// `DELETE /api/children/:id/allowlist/videos/:videoId`.
pub async fn delete_video(
    State(state): State<AppState>,
    Path((child_id, video_id)): Path<(i64, String)>,
) -> AppResult<StatusCode> {
    require_child_id(&state, child_id).await?;
    sqlx::query("DELETE FROM allowlisted_videos WHERE child_account_id = ? AND video_id = ?")
        .bind(child_id)
        .bind(video_id)
        .execute(&state.db)
        .await?;
    Ok(StatusCode::NO_CONTENT)
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

async fn require_child_id(state: &AppState, child_id: i64) -> AppResult<()> {
    if !access::is_child_account(&state.db, child_id).await? {
        return Err(AppError::BadRequest("target account is not a child".into()));
    }
    Ok(())
}

/// Pick the highest-resolution thumbnail URL we have. YouTube returns
/// keyed sizes; "maxres" → "high" → "medium" → "default" → "standard".
pub(crate) fn preferred_thumbnail(
    thumbs: &std::collections::HashMap<String, crate::services::youtube::ThumbnailInfo>,
) -> Option<String> {
    for key in ["maxres", "high", "standard", "medium", "default"] {
        if let Some(t) = thumbs.get(key) {
            return Some(t.url.clone());
        }
    }
    None
}

/// Accept either a raw video ID or a YouTube URL and return the bare ID.
fn parse_video_id(input: &str) -> String {
    let trimmed = input.trim();
    // youtu.be/<id>
    if let Some(rest) = trimmed.strip_prefix("https://youtu.be/") {
        return rest
            .split(['?', '&', '/'])
            .next()
            .unwrap_or(rest)
            .to_string();
    }
    if let Some(rest) = trimmed.strip_prefix("http://youtu.be/") {
        return rest
            .split(['?', '&', '/'])
            .next()
            .unwrap_or(rest)
            .to_string();
    }
    // youtube.com/watch?v=<id>
    if trimmed.contains("youtube.com/watch") {
        if let Some(qpos) = trimmed.find('?') {
            for part in trimmed[qpos + 1..].split('&') {
                if let Some(v) = part.strip_prefix("v=") {
                    return v.to_string();
                }
            }
        }
    }
    // youtube.com/embed/<id> or shorts/<id>
    for prefix in [
        "https://www.youtube.com/embed/",
        "https://www.youtube.com/shorts/",
        "https://youtube.com/embed/",
        "https://youtube.com/shorts/",
    ] {
        if let Some(rest) = trimmed.strip_prefix(prefix) {
            return rest
                .split(['?', '&', '/'])
                .next()
                .unwrap_or(rest)
                .to_string();
        }
    }
    trimmed.to_string()
}
