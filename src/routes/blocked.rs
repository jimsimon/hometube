//! Blocked-videos routes (parent only).
//!
//! A blocked video overrides every allowlist entry — even if the video is
//! in an allowlisted channel, blocking it hides it from the
//! child. Used for one-off "this video, but not the rest of the channel"
//! decisions.

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

#[derive(Debug, Serialize, sqlx::FromRow)]
pub struct BlockedVideo {
    pub id: i64,
    pub video_id: String,
    pub video_title: Option<String>,
    pub reason: Option<String>,
    pub created_at: i64,
}

#[derive(Debug, Deserialize)]
pub struct BlockBody {
    pub video_id: String,
    #[serde(default)]
    pub reason: Option<String>,
}

/// `GET /api/children/:id/blocked`.
pub async fn list(
    State(state): State<AppState>,
    Path(child_id): Path<i64>,
) -> AppResult<Json<Vec<BlockedVideo>>> {
    require_child(&state, child_id).await?;
    let rows: Vec<BlockedVideo> = sqlx::query_as(
        "SELECT id, video_id, video_title, reason, created_at \
         FROM blocked_videos WHERE child_account_id = ? ORDER BY created_at DESC",
    )
    .bind(child_id)
    .fetch_all(&state.db)
    .await?;
    Ok(Json(rows))
}

/// `POST /api/children/:id/blocked`.
pub async fn add(
    State(state): State<AppState>,
    current: CurrentAccount,
    Path(child_id): Path<i64>,
    Json(body): Json<BlockBody>,
) -> AppResult<Json<BlockedVideo>> {
    require_child(&state, child_id).await?;

    // Best-effort metadata lookup so the parent UI shows the title.
    // The `blocked_videos` schema doesn't store a thumbnail (just title
    // + reason), so we don't bother with the thumbnail URL here.
    let title = match YoutubeClient::from_db(&state.db).await {
        Ok(yt) => yt
            .get_video(&body.video_id)
            .await
            .ok()
            .flatten()
            .map(|v| v.title),
        Err(_) => None,
    };

    let row: BlockedVideo = sqlx::query_as(
        "INSERT INTO blocked_videos \
            (child_account_id, video_id, video_title, blocked_by, reason) \
         VALUES (?, ?, ?, ?, ?) \
         ON CONFLICT(child_account_id, video_id) DO UPDATE SET \
            video_title = excluded.video_title, \
            reason = excluded.reason \
         RETURNING id, video_id, video_title, reason, created_at",
    )
    .bind(child_id)
    .bind(&body.video_id)
    .bind(title)
    .bind(current.id)
    .bind(body.reason.clone())
    .fetch_one(&state.db)
    .await?;
    Ok(Json(row))
}

/// `DELETE /api/children/:id/blocked/:videoId`.
pub async fn delete(
    State(state): State<AppState>,
    Path((child_id, video_id)): Path<(i64, String)>,
) -> AppResult<StatusCode> {
    require_child(&state, child_id).await?;
    sqlx::query("DELETE FROM blocked_videos WHERE child_account_id = ? AND video_id = ?")
        .bind(child_id)
        .bind(video_id)
        .execute(&state.db)
        .await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn require_child(state: &AppState, child_id: i64) -> AppResult<()> {
    if !access::is_child_account(&state.db, child_id).await? {
        return Err(AppError::BadRequest("target account is not a child".into()));
    }
    Ok(())
}
