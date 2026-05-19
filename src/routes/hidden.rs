//! Per-child "Hidden Videos" routes (child-only).
//!
//! A child can hide a video they don't want to see. Hidden videos are
//! scoped per-child (one child hiding does NOT affect siblings) and are
//! filtered out of every listing surface by
//! [`crate::services::access::can_child_view`]. The only place a hidden
//! video reappears is the dedicated `/child/hidden` page, which lists
//! the rows in this table and offers an "Unhide" action.
//!
//! Conceptually similar to (but separate from) `blocked_videos`, which
//! is parent-managed moderation.

use axum::{
    extract::{Path, State},
    http::StatusCode,
    Json,
};

use serde::{Deserialize, Serialize};

use crate::error::AppResult;
use crate::middleware::auth::CurrentAccount;
use crate::state::AppState;

#[derive(Debug, Serialize, sqlx::FromRow)]
pub struct HiddenVideo {
    pub id: i64,
    pub video_id: String,
    pub video_title: Option<String>,
    pub channel_id: Option<String>,
    pub channel_title: Option<String>,
    pub video_thumbnail_url: Option<String>,
    pub duration_seconds: Option<i64>,
    pub hidden_at: i64,
}

/// `GET /api/hidden`.
pub async fn list(
    State(state): State<AppState>,
    current: CurrentAccount,
) -> AppResult<Json<Vec<HiddenVideo>>> {
    let rows: Vec<HiddenVideo> = sqlx::query_as(
        "SELECT id, video_id, video_title, channel_id, channel_title, \
                video_thumbnail_url, duration_seconds, hidden_at \
         FROM hidden_videos \
         WHERE child_account_id = ? \
         ORDER BY hidden_at DESC",
    )
    .bind(current.id)
    .fetch_all(&state.db)
    .await?;
    Ok(Json(rows))
}

#[derive(Debug, Deserialize)]
pub struct CreateBody {
    pub video_id: String,
    #[serde(default)]
    pub video_title: Option<String>,
    #[serde(default)]
    pub channel_id: Option<String>,
    #[serde(default)]
    pub channel_title: Option<String>,
    #[serde(default)]
    pub video_thumbnail_url: Option<String>,
    #[serde(default)]
    pub duration_seconds: Option<i64>,
}

/// `POST /api/hidden`.
///
/// Idempotent upsert: re-hiding a video refreshes `hidden_at` and any
/// metadata fields the client now knows. Returns the stored row.
pub async fn add(
    State(state): State<AppState>,
    current: CurrentAccount,
    Json(body): Json<CreateBody>,
) -> AppResult<Json<HiddenVideo>> {
    let row: HiddenVideo = sqlx::query_as(
        "INSERT INTO hidden_videos \
            (child_account_id, video_id, video_title, channel_id, channel_title, \
             video_thumbnail_url, duration_seconds) \
         VALUES (?, ?, ?, ?, ?, ?, ?) \
         ON CONFLICT(child_account_id, video_id) DO UPDATE SET \
            hidden_at = unixepoch(), \
            video_title = COALESCE(excluded.video_title, hidden_videos.video_title), \
            channel_id = COALESCE(excluded.channel_id, hidden_videos.channel_id), \
            channel_title = COALESCE(excluded.channel_title, hidden_videos.channel_title), \
            video_thumbnail_url = COALESCE(excluded.video_thumbnail_url, hidden_videos.video_thumbnail_url), \
            duration_seconds = COALESCE(excluded.duration_seconds, hidden_videos.duration_seconds) \
         RETURNING id, video_id, video_title, channel_id, channel_title, \
                   video_thumbnail_url, duration_seconds, hidden_at",
    )
    .bind(current.id)
    .bind(&body.video_id)
    .bind(&body.video_title)
    .bind(&body.channel_id)
    .bind(&body.channel_title)
    .bind(&body.video_thumbnail_url)
    .bind(body.duration_seconds)
    .fetch_one(&state.db)
    .await?;
    Ok(Json(row))
}

/// `DELETE /api/hidden/:video_id`.
pub async fn remove(
    State(state): State<AppState>,
    current: CurrentAccount,
    Path(video_id): Path<String>,
) -> AppResult<StatusCode> {
    sqlx::query("DELETE FROM hidden_videos WHERE child_account_id = ? AND video_id = ?")
        .bind(current.id)
        .bind(&video_id)
        .execute(&state.db)
        .await?;
    Ok(StatusCode::NO_CONTENT)
}
