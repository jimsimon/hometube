//! Child playlist routes (YouTube-synced).
//!
//! `child_playlists` mirrors the child's playlists; rows have `is_own=1`
//! when the child created the playlist in HomeTube and `is_own=0` when
//! the playlist is a YouTube-side playlist that's been "added to
//! library" via the parent's allowlist.
//!
//! All mutating endpoints write locally first, set the appropriate
//! `sync_status`, and spawn a background task in
//! [`crate::services::sync`] that reconciles with YouTube.

use axum::{
    extract::{Path, State},
    http::StatusCode,
    Json,
};
use serde::{Deserialize, Serialize};

use crate::error::{AppError, AppResult};
use crate::middleware::auth::CurrentAccount;
use crate::services::sync::{push_playlist_change, push_playlist_item_change, PlaylistItemAction};
use crate::services::youtube::YoutubeClient;
use crate::state::AppState;

// ---------------------------------------------------------------------------
// List view
// ---------------------------------------------------------------------------

/// One row in the playlist list view.
#[derive(Debug, Serialize, sqlx::FromRow)]
pub struct PlaylistSummary {
    pub id: i64,
    pub youtube_playlist_id: Option<String>,
    pub title: String,
    pub description: Option<String>,
    pub is_own: bool,
    pub source: String,
    pub sync_status: String,
    pub video_count: i64,
    pub created_at: i64,
    pub updated_at: i64,
}

/// `GET /api/playlists`.
pub async fn list(
    State(state): State<AppState>,
    current: CurrentAccount,
) -> AppResult<Json<Vec<PlaylistSummary>>> {
    let rows: Vec<(i64, Option<String>, String, Option<String>, i64, String, String, i64, i64, i64)> =
        sqlx::query_as(
            "SELECT p.id, p.youtube_playlist_id, p.title, p.description, p.is_own, \
                    p.source, p.sync_status, \
                    (SELECT COUNT(*) FROM child_playlist_videos v WHERE v.playlist_id = p.id) AS video_count, \
                    p.created_at, p.updated_at \
             FROM child_playlists p \
             WHERE p.child_account_id = ? AND p.is_deleted = 0 \
             ORDER BY p.updated_at DESC",
        )
        .bind(current.id)
        .fetch_all(&state.db)
        .await?;
    let out = rows
        .into_iter()
        .map(
            |(
                id,
                yt_id,
                title,
                description,
                is_own,
                source,
                sync_status,
                video_count,
                created_at,
                updated_at,
            )| {
                PlaylistSummary {
                    id,
                    youtube_playlist_id: yt_id,
                    title,
                    description,
                    is_own: is_own != 0,
                    source,
                    sync_status,
                    video_count,
                    created_at,
                    updated_at,
                }
            },
        )
        .collect();
    Ok(Json(out))
}

// ---------------------------------------------------------------------------
// Create
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct CreatePlaylistBody {
    pub title: String,
    #[serde(default)]
    pub description: Option<String>,
}

/// `POST /api/playlists`.
pub async fn create(
    State(state): State<AppState>,
    current: CurrentAccount,
    Json(body): Json<CreatePlaylistBody>,
) -> AppResult<Json<PlaylistSummary>> {
    let title = body.title.trim();
    if title.is_empty() {
        return Err(AppError::BadRequest("title must not be empty".into()));
    }

    let id: i64 = sqlx::query_scalar(
        "INSERT INTO child_playlists \
            (child_account_id, title, description, is_own, source, sync_status, is_deleted) \
         VALUES (?, ?, ?, 1, 'app', 'pending_create', 0) \
         RETURNING id",
    )
    .bind(current.id)
    .bind(title)
    .bind(&body.description)
    .fetch_one(&state.db)
    .await?;

    spawn_playlist_push(state.clone(), current.id, id);

    let row = fetch_summary(&state, id).await?;
    Ok(Json(row))
}

// ---------------------------------------------------------------------------
// Detail (single playlist + its videos)
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize, sqlx::FromRow)]
pub struct PlaylistVideo {
    pub id: i64,
    pub video_id: String,
    pub video_title: String,
    pub video_thumbnail_url: Option<String>,
    pub channel_title: Option<String>,
    pub position: i64,
    pub added_at: i64,
}

#[derive(Debug, Serialize)]
pub struct PlaylistDetail {
    #[serde(flatten)]
    pub summary: PlaylistSummary,
    pub videos: Vec<PlaylistVideo>,
}

/// `GET /api/playlists/:id`.
pub async fn detail(
    State(state): State<AppState>,
    current: CurrentAccount,
    Path(playlist_id): Path<i64>,
) -> AppResult<Json<PlaylistDetail>> {
    require_owner(&state, current.id, playlist_id).await?;
    let summary = fetch_summary(&state, playlist_id).await?;
    let videos: Vec<PlaylistVideo> = sqlx::query_as(
        "SELECT id, video_id, video_title, video_thumbnail_url, channel_title, position, added_at \
         FROM child_playlist_videos \
         WHERE playlist_id = ? \
         ORDER BY position",
    )
    .bind(playlist_id)
    .fetch_all(&state.db)
    .await?;
    Ok(Json(PlaylistDetail { summary, videos }))
}

// ---------------------------------------------------------------------------
// Update / delete
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct UpdatePlaylistBody {
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub description: Option<Option<String>>,
}

/// `PUT /api/playlists/:id`.
pub async fn update(
    State(state): State<AppState>,
    current: CurrentAccount,
    Path(playlist_id): Path<i64>,
    Json(body): Json<UpdatePlaylistBody>,
) -> AppResult<Json<PlaylistSummary>> {
    require_owner(&state, current.id, playlist_id).await?;
    let is_own = is_own(&state, playlist_id).await?;
    if !is_own {
        return Err(AppError::BadRequest(
            "library playlists are read-only".into(),
        ));
    }

    if let Some(title) = body.title.as_ref() {
        let trimmed = title.trim();
        if trimmed.is_empty() {
            return Err(AppError::BadRequest("title must not be empty".into()));
        }
        sqlx::query(
            "UPDATE child_playlists SET title = ?, sync_status = 'pending_update', updated_at = unixepoch() \
             WHERE id = ?",
        )
        .bind(trimmed)
        .bind(playlist_id)
        .execute(&state.db)
        .await?;
    }
    if let Some(desc) = body.description.as_ref() {
        sqlx::query(
            "UPDATE child_playlists SET description = ?, sync_status = 'pending_update', updated_at = unixepoch() \
             WHERE id = ?",
        )
        .bind(desc.as_ref())
        .bind(playlist_id)
        .execute(&state.db)
        .await?;
    }

    spawn_playlist_push(state.clone(), current.id, playlist_id);

    Ok(Json(fetch_summary(&state, playlist_id).await?))
}

/// `DELETE /api/playlists/:id` — soft delete.
pub async fn delete(
    State(state): State<AppState>,
    current: CurrentAccount,
    Path(playlist_id): Path<i64>,
) -> AppResult<StatusCode> {
    require_owner(&state, current.id, playlist_id).await?;
    let is_own = is_own(&state, playlist_id).await?;
    sqlx::query(
        "UPDATE child_playlists \
         SET is_deleted = 1, \
             sync_status = CASE WHEN ? = 1 THEN 'pending_delete' ELSE 'synced' END, \
             updated_at = unixepoch() \
         WHERE id = ?",
    )
    .bind(is_own as i64)
    .bind(playlist_id)
    .execute(&state.db)
    .await?;
    if is_own {
        spawn_playlist_push(state.clone(), current.id, playlist_id);
    }
    Ok(StatusCode::NO_CONTENT)
}

// ---------------------------------------------------------------------------
// Add / remove / reorder videos
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct AddVideoBody {
    pub video_id: String,
}

/// `POST /api/playlists/:id/videos` — append a video to the playlist.
pub async fn add_video(
    State(state): State<AppState>,
    current: CurrentAccount,
    Path(playlist_id): Path<i64>,
    Json(body): Json<AddVideoBody>,
) -> AppResult<Json<PlaylistVideo>> {
    require_owner(&state, current.id, playlist_id).await?;
    if !is_own(&state, playlist_id).await? {
        return Err(AppError::BadRequest(
            "library playlists are read-only".into(),
        ));
    }

    // Resolve a usable title/thumbnail/channel for the row.
    let yt = YoutubeClient::from_db(&state.db).await?;
    let info = yt
        .get_video(&body.video_id)
        .await?
        .ok_or_else(|| AppError::BadRequest("video not found on YouTube".into()))?;
    let thumb = info
        .thumbnails
        .get("maxres")
        .or_else(|| info.thumbnails.get("high"))
        .or_else(|| info.thumbnails.get("standard"))
        .or_else(|| info.thumbnails.get("medium"))
        .or_else(|| info.thumbnails.get("default"))
        .map(|t| t.url.clone());

    // Compute the next free position.
    let next_position: i64 = sqlx::query_scalar(
        "SELECT COALESCE(MAX(position), -1) + 1 FROM child_playlist_videos \
         WHERE playlist_id = ?",
    )
    .bind(playlist_id)
    .fetch_one(&state.db)
    .await?;

    let row: PlaylistVideo = sqlx::query_as(
        "INSERT INTO child_playlist_videos \
            (playlist_id, video_id, video_title, video_thumbnail_url, channel_title, position) \
         VALUES (?, ?, ?, ?, ?, ?) \
         ON CONFLICT(playlist_id, video_id) DO UPDATE SET \
            video_title = excluded.video_title, \
            video_thumbnail_url = excluded.video_thumbnail_url, \
            channel_title = excluded.channel_title \
         RETURNING id, video_id, video_title, video_thumbnail_url, channel_title, position, added_at",
    )
    .bind(playlist_id)
    .bind(&info.id)
    .bind(&info.title)
    .bind(thumb)
    .bind(info.channel_title.clone())
    .bind(next_position)
    .fetch_one(&state.db)
    .await?;

    sqlx::query(
        "UPDATE child_playlists SET sync_status = 'pending_update', updated_at = unixepoch() \
         WHERE id = ?",
    )
    .bind(playlist_id)
    .execute(&state.db)
    .await?;

    spawn_item_push(
        state.clone(),
        current.id,
        playlist_id,
        info.id.clone(),
        PlaylistItemAction::Add,
    );

    Ok(Json(row))
}

/// `DELETE /api/playlists/:id/videos/:videoId`.
pub async fn remove_video(
    State(state): State<AppState>,
    current: CurrentAccount,
    Path((playlist_id, video_id)): Path<(i64, String)>,
) -> AppResult<StatusCode> {
    require_owner(&state, current.id, playlist_id).await?;
    if !is_own(&state, playlist_id).await? {
        return Err(AppError::BadRequest(
            "library playlists are read-only".into(),
        ));
    }
    let result =
        sqlx::query("DELETE FROM child_playlist_videos WHERE playlist_id = ? AND video_id = ?")
            .bind(playlist_id)
            .bind(&video_id)
            .execute(&state.db)
            .await?;
    if result.rows_affected() == 0 {
        return Err(AppError::NotFound);
    }

    sqlx::query(
        "UPDATE child_playlists SET sync_status = 'pending_update', updated_at = unixepoch() \
         WHERE id = ?",
    )
    .bind(playlist_id)
    .execute(&state.db)
    .await?;

    spawn_item_push(
        state.clone(),
        current.id,
        playlist_id,
        video_id,
        PlaylistItemAction::Remove,
    );
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Debug, Deserialize)]
pub struct ReorderBody {
    pub video_ids: Vec<String>,
}

/// `PUT /api/playlists/:id/videos/reorder`.
///
/// Replaces the `position` of every row in a single transaction. The
/// background sync attempts a YouTube reorder via
/// `playlistItems.update`; YouTube refuses reorder on system playlists
/// (LL/WL) — for those we fail with a 400 immediately.
pub async fn reorder_videos(
    State(state): State<AppState>,
    current: CurrentAccount,
    Path(playlist_id): Path<i64>,
    Json(body): Json<ReorderBody>,
) -> AppResult<Json<Vec<PlaylistVideo>>> {
    require_owner(&state, current.id, playlist_id).await?;
    let is_own = is_own(&state, playlist_id).await?;
    if !is_own {
        return Err(AppError::BadRequest(
            "library playlists are read-only".into(),
        ));
    }
    let yt_id: Option<String> =
        sqlx::query_scalar("SELECT youtube_playlist_id FROM child_playlists WHERE id = ?")
            .bind(playlist_id)
            .fetch_optional(&state.db)
            .await?
            .flatten();
    if let Some(id) = yt_id.as_deref() {
        // YouTube refuses reorder on system playlists.
        if id == "LL" || id == "WL" {
            return Err(AppError::BadRequest(
                "reorder not supported on this playlist".into(),
            ));
        }
    }

    // The schema has no UNIQUE constraint on `(playlist_id, position)`,
    // so a single update pass is safe even when positions transiently
    // collide.
    let mut tx = state.db.begin().await?;
    for (position, video_id) in body.video_ids.iter().enumerate() {
        sqlx::query(
            "UPDATE child_playlist_videos SET position = ? \
             WHERE playlist_id = ? AND video_id = ?",
        )
        .bind(position as i64)
        .bind(playlist_id)
        .bind(video_id)
        .execute(&mut *tx)
        .await?;
    }
    sqlx::query(
        "UPDATE child_playlists SET sync_status = 'pending_update', updated_at = unixepoch() \
         WHERE id = ?",
    )
    .bind(playlist_id)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;

    spawn_item_push(
        state.clone(),
        current.id,
        playlist_id,
        String::new(),
        PlaylistItemAction::Reorder,
    );

    let videos: Vec<PlaylistVideo> = sqlx::query_as(
        "SELECT id, video_id, video_title, video_thumbnail_url, channel_title, position, added_at \
         FROM child_playlist_videos WHERE playlist_id = ? ORDER BY position",
    )
    .bind(playlist_id)
    .fetch_all(&state.db)
    .await?;
    Ok(Json(videos))
}

// ---------------------------------------------------------------------------
// Add allowlisted YouTube playlist to library
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct AddLibraryBody {
    pub youtube_playlist_id: String,
}

/// `POST /api/playlists/library` — import a YouTube playlist into the
/// child's library. The playlist must already be allowlisted by the
/// parent; otherwise the request is refused.
pub async fn add_library(
    State(state): State<AppState>,
    current: CurrentAccount,
    Json(body): Json<AddLibraryBody>,
) -> AppResult<Json<PlaylistSummary>> {
    let allowlisted: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM allowlisted_playlists \
         WHERE child_account_id = ? AND playlist_id = ?",
    )
    .bind(current.id)
    .bind(&body.youtube_playlist_id)
    .fetch_one(&state.db)
    .await?;
    if allowlisted == 0 {
        return Err(AppError::Forbidden);
    }

    let yt = YoutubeClient::from_db(&state.db).await?;
    let info = yt
        .get_playlist(&body.youtube_playlist_id)
        .await?
        .ok_or_else(|| AppError::BadRequest("playlist not found on YouTube".into()))?;

    // Manual duplicate check — `child_playlists` has no UNIQUE
    // constraint on `(child_account_id, youtube_playlist_id)` because
    // app-created playlists may not yet have a YouTube ID. If a soft-
    // deleted row already exists, resurrect it and refresh metadata.
    let existing: Option<(i64, i64)> = sqlx::query_as(
        "SELECT id, is_deleted FROM child_playlists \
         WHERE child_account_id = ? AND youtube_playlist_id = ?",
    )
    .bind(current.id)
    .bind(&info.id)
    .fetch_optional(&state.db)
    .await?;
    if let Some((id, _)) = existing {
        sqlx::query(
            "UPDATE child_playlists SET is_deleted = 0, source = 'youtube', \
                                          is_own = 0, sync_status = 'synced', \
                                          title = ?, description = ?, \
                                          updated_at = unixepoch() \
             WHERE id = ?",
        )
        .bind(&info.title)
        .bind(&info.description)
        .bind(id)
        .execute(&state.db)
        .await?;
        return Ok(Json(fetch_summary(&state, id).await?));
    }

    let id: i64 = sqlx::query_scalar(
        "INSERT INTO child_playlists \
            (child_account_id, youtube_playlist_id, title, description, is_own, source, sync_status, is_deleted) \
         VALUES (?, ?, ?, ?, 0, 'youtube', 'synced', 0) \
         RETURNING id",
    )
    .bind(current.id)
    .bind(&info.id)
    .bind(&info.title)
    .bind(&info.description)
    .fetch_one(&state.db)
    .await?;
    Ok(Json(fetch_summary(&state, id).await?))
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

async fn require_owner(state: &AppState, child_id: i64, playlist_id: i64) -> AppResult<()> {
    let count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM child_playlists \
         WHERE id = ? AND child_account_id = ? AND is_deleted = 0",
    )
    .bind(playlist_id)
    .bind(child_id)
    .fetch_one(&state.db)
    .await?;
    if count == 0 {
        Err(AppError::NotFound)
    } else {
        Ok(())
    }
}

async fn is_own(state: &AppState, playlist_id: i64) -> AppResult<bool> {
    let v: i64 = sqlx::query_scalar("SELECT is_own FROM child_playlists WHERE id = ?")
        .bind(playlist_id)
        .fetch_one(&state.db)
        .await?;
    Ok(v != 0)
}

async fn fetch_summary(state: &AppState, playlist_id: i64) -> AppResult<PlaylistSummary> {
    let row: (i64, Option<String>, String, Option<String>, i64, String, String, i64, i64, i64) =
        sqlx::query_as(
            "SELECT p.id, p.youtube_playlist_id, p.title, p.description, p.is_own, \
                    p.source, p.sync_status, \
                    (SELECT COUNT(*) FROM child_playlist_videos v WHERE v.playlist_id = p.id) AS video_count, \
                    p.created_at, p.updated_at \
             FROM child_playlists p WHERE p.id = ?",
        )
        .bind(playlist_id)
        .fetch_one(&state.db)
        .await?;
    let (
        id,
        yt_id,
        title,
        description,
        is_own,
        source,
        sync_status,
        video_count,
        created_at,
        updated_at,
    ) = row;
    Ok(PlaylistSummary {
        id,
        youtube_playlist_id: yt_id,
        title,
        description,
        is_own: is_own != 0,
        source,
        sync_status,
        video_count,
        created_at,
        updated_at,
    })
}

fn spawn_playlist_push(state: AppState, account_id: i64, playlist_id: i64) {
    tokio::spawn(async move {
        if let Err(err) = push_playlist_change(&state.db, account_id, playlist_id).await {
            tracing::warn!(account_id, playlist_id, %err, "playlist push failed");
        }
    });
}

fn spawn_item_push(
    state: AppState,
    account_id: i64,
    playlist_id: i64,
    video_id: String,
    action: PlaylistItemAction,
) {
    tokio::spawn(async move {
        if let Err(err) =
            push_playlist_item_change(&state.db, account_id, playlist_id, &video_id, action).await
        {
            tracing::warn!(
                account_id,
                playlist_id,
                %video_id,
                %err,
                "playlist-item push failed"
            );
        }
    });
}
