//! Child like routes.
//!
//! Likes are mirrored locally in `video_likes` and pushed to YouTube via
//! the `videos.rate` endpoint. The local UI updates immediately; a
//! background task reconciles with YouTube using
//! [`crate::services::sync::push_like_change`].

use axum::{
    extract::{Path, State},
    http::StatusCode,
    Json,
};
use serde::Serialize;

use crate::error::{AppError, AppResult};
use crate::middleware::auth::CurrentAccount;
use crate::services::sync::push_like_change;
use crate::services::youtube::YoutubeClient;
use crate::state::AppState;

#[derive(Debug, Serialize, sqlx::FromRow)]
pub struct LikeRow {
    pub id: i64,
    pub video_id: String,
    pub video_title: Option<String>,
    pub video_thumbnail_url: Option<String>,
    pub source: String,
    pub sync_status: String,
    pub liked_at: i64,
}

/// `GET /api/likes`.
pub async fn list(
    State(state): State<AppState>,
    current: CurrentAccount,
) -> AppResult<Json<Vec<LikeRow>>> {
    let rows: Vec<LikeRow> = sqlx::query_as(
        "SELECT id, video_id, video_title, video_thumbnail_url, source, sync_status, liked_at \
         FROM video_likes \
         WHERE child_account_id = ? AND is_deleted = 0 \
         ORDER BY liked_at DESC",
    )
    .bind(current.id)
    .fetch_all(&state.db)
    .await?;
    Ok(Json(rows))
}

/// `POST /api/likes/:videoId`.
pub async fn like(
    State(state): State<AppState>,
    current: CurrentAccount,
    Path(video_id): Path<String>,
) -> AppResult<Json<LikeRow>> {
    // Best-effort metadata. Don't fail the request if YouTube lookup
    // breaks — the sync task will fill in details.
    let (title, thumb) = match YoutubeClient::from_db(&state.db).await {
        Ok(yt) => match yt.get_video(&video_id).await.ok().flatten() {
            Some(info) => {
                let thumb = info
                    .thumbnails
                    .get("maxres")
                    .or_else(|| info.thumbnails.get("high"))
                    .or_else(|| info.thumbnails.get("medium"))
                    .or_else(|| info.thumbnails.get("default"))
                    .map(|t| t.url.clone());
                (Some(info.title), thumb)
            }
            None => (None, None),
        },
        Err(_) => (None, None),
    };

    sqlx::query(
        "INSERT INTO video_likes \
            (child_account_id, video_id, video_title, video_thumbnail_url, source, sync_status, is_deleted) \
         VALUES (?, ?, ?, ?, 'app', 'pending_push', 0) \
         ON CONFLICT(child_account_id, video_id) DO UPDATE SET \
            video_title = COALESCE(excluded.video_title, video_likes.video_title), \
            video_thumbnail_url = COALESCE(excluded.video_thumbnail_url, video_likes.video_thumbnail_url), \
            source = 'app', \
            sync_status = 'pending_push', \
            is_deleted = 0, \
            updated_at = unixepoch()",
    )
    .bind(current.id)
    .bind(&video_id)
    .bind(&title)
    .bind(&thumb)
    .execute(&state.db)
    .await?;

    spawn_push(state.clone(), current.id, video_id.clone());

    let row: LikeRow = sqlx::query_as(
        "SELECT id, video_id, video_title, video_thumbnail_url, source, sync_status, liked_at \
         FROM video_likes WHERE child_account_id = ? AND video_id = ?",
    )
    .bind(current.id)
    .bind(&video_id)
    .fetch_one(&state.db)
    .await?;
    Ok(Json(row))
}

/// `DELETE /api/likes/:videoId`.
pub async fn unlike(
    State(state): State<AppState>,
    current: CurrentAccount,
    Path(video_id): Path<String>,
) -> AppResult<StatusCode> {
    let result = sqlx::query(
        "UPDATE video_likes \
         SET is_deleted = 1, sync_status = 'pending_delete', updated_at = unixepoch() \
         WHERE child_account_id = ? AND video_id = ?",
    )
    .bind(current.id)
    .bind(&video_id)
    .execute(&state.db)
    .await?;
    if result.rows_affected() == 0 {
        return Err(AppError::NotFound);
    }
    spawn_push(state.clone(), current.id, video_id);
    Ok(StatusCode::NO_CONTENT)
}

fn spawn_push(state: AppState, account_id: i64, video_id: String) {
    tokio::spawn(async move {
        if let Err(err) = push_like_change(&state.db, account_id, &video_id).await {
            tracing::warn!(account_id, %video_id, %err, "like push failed");
        }
    });
}
