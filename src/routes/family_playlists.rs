//! Family-playlist routes (Phase 18).
//!
//! Family playlists are parent-created lists shared with a curated set
//! of children. They live in three tables:
//!
//! - `family_playlists` — the playlist itself
//! - `family_playlist_members` — which children can see it
//! - `family_playlist_videos` — ordered videos
//!
//! Parents can mutate everything; children see only the playlists they
//! were assigned to and the videos within (read-only).

use axum::{
    extract::{Path, State},
    http::StatusCode,
    Json,
};
use serde::{Deserialize, Serialize};
use sqlx::Row;

use crate::error::{AppError, AppResult};
use crate::middleware::auth::CurrentAccount;
use crate::models::account::AccountType;
use crate::services::youtube::YoutubeClient;
use crate::state::AppState;

#[derive(Debug, Serialize, sqlx::FromRow)]
pub struct FamilyPlaylistSummary {
    pub id: i64,
    pub created_by: i64,
    pub title: String,
    pub description: Option<String>,
    pub created_at: i64,
    pub updated_at: i64,
    pub video_count: i64,
}

#[derive(Debug, Serialize, sqlx::FromRow)]
pub struct FamilyPlaylistVideo {
    pub id: i64,
    pub video_id: String,
    pub video_title: String,
    pub video_thumbnail_url: Option<String>,
    pub channel_title: Option<String>,
    pub position: i64,
    pub added_at: i64,
}

#[derive(Debug, Serialize)]
pub struct FamilyPlaylistDetail {
    #[serde(flatten)]
    pub summary: FamilyPlaylistSummary,
    pub videos: Vec<FamilyPlaylistVideo>,
    pub child_ids: Vec<i64>,
}

/// `GET /api/family-playlists`.
///
/// - **Parent**: lists every family playlist they created (or any
///   parent created — there's only one family per instance).
/// - **Child**: lists the playlists they were assigned to.
pub async fn list(
    State(state): State<AppState>,
    current: CurrentAccount,
) -> AppResult<Json<Vec<FamilyPlaylistSummary>>> {
    let rows: Vec<FamilyPlaylistSummary> = match current.account_type {
        AccountType::Parent => {
            sqlx::query_as(
                "SELECT fp.id, fp.created_by, fp.title, fp.description, \
                        fp.created_at, fp.updated_at, \
                        (SELECT COUNT(*) FROM family_playlist_videos v WHERE v.playlist_id = fp.id) as video_count \
                 FROM family_playlists fp \
                 ORDER BY fp.updated_at DESC",
            )
            .fetch_all(&state.db)
            .await?
        }
        AccountType::Child => {
            sqlx::query_as(
                "SELECT fp.id, fp.created_by, fp.title, fp.description, \
                        fp.created_at, fp.updated_at, \
                        (SELECT COUNT(*) FROM family_playlist_videos v WHERE v.playlist_id = fp.id) as video_count \
                 FROM family_playlists fp \
                 INNER JOIN family_playlist_members m ON m.playlist_id = fp.id \
                 WHERE m.child_account_id = ? \
                 ORDER BY fp.updated_at DESC",
            )
            .bind(current.id)
            .fetch_all(&state.db)
            .await?
        }
    };
    Ok(Json(rows))
}

#[derive(Debug, Deserialize)]
pub struct CreateBody {
    pub title: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub child_ids: Vec<i64>,
}

/// `POST /api/family-playlists` (parent-only).
pub async fn create(
    State(state): State<AppState>,
    current: CurrentAccount,
    Json(body): Json<CreateBody>,
) -> AppResult<Json<FamilyPlaylistDetail>> {
    require_parent(&current)?;
    if body.title.trim().is_empty() {
        return Err(AppError::BadRequest("title is required".into()));
    }

    let mut tx = state.db.begin().await?;
    let id: i64 = sqlx::query_scalar(
        "INSERT INTO family_playlists (created_by, title, description) \
         VALUES (?, ?, ?) RETURNING id",
    )
    .bind(current.id)
    .bind(body.title.trim())
    .bind(body.description.as_deref())
    .fetch_one(&mut *tx)
    .await?;

    for child_id in &body.child_ids {
        if !child_exists(&state, *child_id).await? {
            tx.rollback().await.ok();
            return Err(AppError::BadRequest(format!(
                "child_id {child_id} does not exist"
            )));
        }
        sqlx::query(
            "INSERT OR IGNORE INTO family_playlist_members \
                (playlist_id, child_account_id) VALUES (?, ?)",
        )
        .bind(id)
        .bind(*child_id)
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;

    let detail = load_detail(&state, id).await?;
    Ok(Json(detail))
}

/// `GET /api/family-playlists/:id`.
pub async fn detail(
    State(state): State<AppState>,
    current: CurrentAccount,
    Path(id): Path<i64>,
) -> AppResult<Json<FamilyPlaylistDetail>> {
    enforce_visible(&state, &current, id).await?;
    Ok(Json(load_detail(&state, id).await?))
}

#[derive(Debug, Deserialize)]
pub struct UpdateBody {
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    /// When present, replaces the member set entirely.
    #[serde(default)]
    pub child_ids: Option<Vec<i64>>,
}

/// `PUT /api/family-playlists/:id` (parent-only).
pub async fn update(
    State(state): State<AppState>,
    current: CurrentAccount,
    Path(id): Path<i64>,
    Json(body): Json<UpdateBody>,
) -> AppResult<Json<FamilyPlaylistDetail>> {
    require_parent(&current)?;
    ensure_exists(&state, id).await?;

    let mut tx = state.db.begin().await?;
    if let Some(title) = body.title.as_deref() {
        if title.trim().is_empty() {
            return Err(AppError::BadRequest("title cannot be empty".into()));
        }
        sqlx::query("UPDATE family_playlists SET title = ?, updated_at = unixepoch() WHERE id = ?")
            .bind(title.trim())
            .bind(id)
            .execute(&mut *tx)
            .await?;
    }
    if body.description.is_some() {
        sqlx::query(
            "UPDATE family_playlists SET description = ?, updated_at = unixepoch() WHERE id = ?",
        )
        .bind(body.description.as_deref())
        .bind(id)
        .execute(&mut *tx)
        .await?;
    }
    if let Some(child_ids) = &body.child_ids {
        sqlx::query("DELETE FROM family_playlist_members WHERE playlist_id = ?")
            .bind(id)
            .execute(&mut *tx)
            .await?;
        for cid in child_ids {
            sqlx::query(
                "INSERT OR IGNORE INTO family_playlist_members \
                    (playlist_id, child_account_id) VALUES (?, ?)",
            )
            .bind(id)
            .bind(*cid)
            .execute(&mut *tx)
            .await?;
        }
    }
    tx.commit().await?;

    Ok(Json(load_detail(&state, id).await?))
}

/// `DELETE /api/family-playlists/:id` (parent-only).
pub async fn delete(
    State(state): State<AppState>,
    current: CurrentAccount,
    Path(id): Path<i64>,
) -> AppResult<StatusCode> {
    require_parent(&current)?;
    let res = sqlx::query("DELETE FROM family_playlists WHERE id = ?")
        .bind(id)
        .execute(&state.db)
        .await?;
    if res.rows_affected() == 0 {
        return Err(AppError::NotFound);
    }
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Debug, Deserialize)]
pub struct AddVideoBody {
    pub video_id: String,
}

/// `POST /api/family-playlists/:id/videos` (parent-only).
///
/// Pulls metadata for the video from the YouTube Data API so the row
/// has a meaningful title/thumbnail without forcing the parent to type
/// it in.
pub async fn add_video(
    State(state): State<AppState>,
    current: CurrentAccount,
    Path(id): Path<i64>,
    Json(body): Json<AddVideoBody>,
) -> AppResult<Json<FamilyPlaylistVideo>> {
    require_parent(&current)?;
    ensure_exists(&state, id).await?;

    let video_id = body.video_id.trim();
    if video_id.is_empty() {
        return Err(AppError::BadRequest("video_id is required".into()));
    }

    let yt = YoutubeClient::from_db(&state.db).await?;
    let info = yt
        .get_video(video_id)
        .await?
        .ok_or_else(|| AppError::NotFound)?;
    let thumb = info
        .thumbnails
        .get("maxres")
        .or_else(|| info.thumbnails.get("high"))
        .or_else(|| info.thumbnails.get("medium"))
        .or_else(|| info.thumbnails.get("default"))
        .map(|t| t.url.clone());

    let next_position: i64 = sqlx::query_scalar(
        "SELECT COALESCE(MAX(position), -1) + 1 FROM family_playlist_videos WHERE playlist_id = ?",
    )
    .bind(id)
    .fetch_one(&state.db)
    .await?;

    let row: FamilyPlaylistVideo = sqlx::query_as(
        "INSERT INTO family_playlist_videos \
            (playlist_id, video_id, video_title, video_thumbnail_url, channel_title, position) \
         VALUES (?, ?, ?, ?, ?, ?) \
         ON CONFLICT(playlist_id, video_id) DO UPDATE SET \
            video_title = excluded.video_title, \
            video_thumbnail_url = excluded.video_thumbnail_url, \
            channel_title = excluded.channel_title \
         RETURNING id, video_id, video_title, video_thumbnail_url, channel_title, position, added_at",
    )
    .bind(id)
    .bind(video_id)
    .bind(&info.title)
    .bind(&thumb)
    .bind(info.channel_title.as_deref())
    .bind(next_position)
    .fetch_one(&state.db)
    .await?;

    sqlx::query("UPDATE family_playlists SET updated_at = unixepoch() WHERE id = ?")
        .bind(id)
        .execute(&state.db)
        .await
        .ok();

    Ok(Json(row))
}

/// `DELETE /api/family-playlists/:id/videos/:videoId` (parent-only).
pub async fn remove_video(
    State(state): State<AppState>,
    current: CurrentAccount,
    Path((id, video_id)): Path<(i64, String)>,
) -> AppResult<StatusCode> {
    require_parent(&current)?;
    let res =
        sqlx::query("DELETE FROM family_playlist_videos WHERE playlist_id = ? AND video_id = ?")
            .bind(id)
            .bind(&video_id)
            .execute(&state.db)
            .await?;
    if res.rows_affected() == 0 {
        return Err(AppError::NotFound);
    }
    sqlx::query("UPDATE family_playlists SET updated_at = unixepoch() WHERE id = ?")
        .bind(id)
        .execute(&state.db)
        .await
        .ok();
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Debug, Deserialize)]
pub struct ReorderBody {
    pub video_ids: Vec<String>,
}

/// `PUT /api/family-playlists/:id/videos/reorder` (parent-only).
///
/// Replaces every video's `position` in a single transaction so the
/// list is reordered atomically.
pub async fn reorder(
    State(state): State<AppState>,
    current: CurrentAccount,
    Path(id): Path<i64>,
    Json(body): Json<ReorderBody>,
) -> AppResult<StatusCode> {
    require_parent(&current)?;
    ensure_exists(&state, id).await?;

    let mut tx = state.db.begin().await?;
    for (idx, vid) in body.video_ids.iter().enumerate() {
        sqlx::query(
            "UPDATE family_playlist_videos SET position = ? \
             WHERE playlist_id = ? AND video_id = ?",
        )
        .bind(idx as i64)
        .bind(id)
        .bind(vid)
        .execute(&mut *tx)
        .await?;
    }
    sqlx::query("UPDATE family_playlists SET updated_at = unixepoch() WHERE id = ?")
        .bind(id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(StatusCode::NO_CONTENT)
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn require_parent(current: &CurrentAccount) -> AppResult<()> {
    if matches!(current.account_type, AccountType::Parent) {
        Ok(())
    } else {
        Err(AppError::Forbidden)
    }
}

async fn ensure_exists(state: &AppState, id: i64) -> AppResult<()> {
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM family_playlists WHERE id = ?")
        .bind(id)
        .fetch_one(&state.db)
        .await?;
    if count == 0 {
        Err(AppError::NotFound)
    } else {
        Ok(())
    }
}

async fn child_exists(state: &AppState, child_id: i64) -> AppResult<bool> {
    let count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM accounts WHERE id = ? AND account_type = 'child'")
            .bind(child_id)
            .fetch_one(&state.db)
            .await?;
    Ok(count > 0)
}

async fn enforce_visible(state: &AppState, current: &CurrentAccount, id: i64) -> AppResult<()> {
    if matches!(current.account_type, AccountType::Parent) {
        ensure_exists(state, id).await?;
        return Ok(());
    }
    // Child: must be a member of this playlist.
    let count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM family_playlist_members \
         WHERE playlist_id = ? AND child_account_id = ?",
    )
    .bind(id)
    .bind(current.id)
    .fetch_one(&state.db)
    .await?;
    if count == 0 {
        return Err(AppError::NotFound);
    }
    Ok(())
}

async fn load_detail(state: &AppState, id: i64) -> AppResult<FamilyPlaylistDetail> {
    let summary: FamilyPlaylistSummary = sqlx::query_as(
        "SELECT fp.id, fp.created_by, fp.title, fp.description, \
                fp.created_at, fp.updated_at, \
                (SELECT COUNT(*) FROM family_playlist_videos v WHERE v.playlist_id = fp.id) as video_count \
         FROM family_playlists fp WHERE fp.id = ?",
    )
    .bind(id)
    .fetch_optional(&state.db)
    .await?
    .ok_or(AppError::NotFound)?;

    let videos: Vec<FamilyPlaylistVideo> = sqlx::query_as(
        "SELECT id, video_id, video_title, video_thumbnail_url, channel_title, position, added_at \
         FROM family_playlist_videos WHERE playlist_id = ? \
         ORDER BY position ASC",
    )
    .bind(id)
    .fetch_all(&state.db)
    .await?;

    let child_id_rows =
        sqlx::query("SELECT child_account_id FROM family_playlist_members WHERE playlist_id = ?")
            .bind(id)
            .fetch_all(&state.db)
            .await?;
    let child_ids = child_id_rows.iter().map(|r| r.get::<i64, _>(0)).collect();

    Ok(FamilyPlaylistDetail {
        summary,
        videos,
        child_ids,
    })
}
