//! Video bookmark routes (child-only).
//!
//! Bookmarks let a child save a timestamp inside a video, optionally
//! with a label, and jump back to it later. They live in the
//! `video_bookmarks` table and are unique on
//! `(child_account_id, video_id, timestamp_seconds)` — re-bookmarking the
//! same instant returns the existing row instead of erroring.

use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    Json,
};
use serde::{Deserialize, Serialize};

use crate::error::{AppError, AppResult};
use crate::middleware::auth::CurrentAccount;
use crate::state::AppState;

/// Default page size when listing all bookmarks for a child.
const DEFAULT_PAGE_SIZE: i64 = 50;

#[derive(Debug, Serialize, sqlx::FromRow)]
pub struct Bookmark {
    pub id: i64,
    pub video_id: String,
    pub video_title: Option<String>,
    pub timestamp_seconds: i64,
    pub label: Option<String>,
    pub created_at: i64,
}

#[derive(Debug, Deserialize)]
pub struct ListQuery {
    #[serde(default)]
    pub limit: Option<i64>,
    #[serde(default)]
    pub offset: Option<i64>,
}

/// `GET /api/bookmarks`.
pub async fn list(
    State(state): State<AppState>,
    current: CurrentAccount,
    Query(q): Query<ListQuery>,
) -> AppResult<Json<Vec<Bookmark>>> {
    let limit = q.limit.unwrap_or(DEFAULT_PAGE_SIZE).clamp(1, 200);
    let offset = q.offset.unwrap_or(0).max(0);
    let rows: Vec<Bookmark> = sqlx::query_as(
        "SELECT id, video_id, video_title, timestamp_seconds, label, created_at \
         FROM video_bookmarks \
         WHERE child_account_id = ? \
         ORDER BY created_at DESC \
         LIMIT ? OFFSET ?",
    )
    .bind(current.id)
    .bind(limit)
    .bind(offset)
    .fetch_all(&state.db)
    .await?;
    Ok(Json(rows))
}

/// `GET /api/bookmarks/:videoId`.
pub async fn list_for_video(
    State(state): State<AppState>,
    current: CurrentAccount,
    Path(video_id): Path<String>,
) -> AppResult<Json<Vec<Bookmark>>> {
    let rows: Vec<Bookmark> = sqlx::query_as(
        "SELECT id, video_id, video_title, timestamp_seconds, label, created_at \
         FROM video_bookmarks \
         WHERE child_account_id = ? AND video_id = ? \
         ORDER BY timestamp_seconds ASC",
    )
    .bind(current.id)
    .bind(&video_id)
    .fetch_all(&state.db)
    .await?;
    Ok(Json(rows))
}

#[derive(Debug, Deserialize)]
pub struct CreateBody {
    pub video_id: String,
    pub timestamp_seconds: i64,
    #[serde(default)]
    pub video_title: Option<String>,
    #[serde(default)]
    pub label: Option<String>,
}

/// `POST /api/bookmarks`.
///
/// Conflict on `(child_account_id, video_id, timestamp_seconds)` is
/// fine — the existing row is updated with the new label and returned.
pub async fn create(
    State(state): State<AppState>,
    current: CurrentAccount,
    Json(body): Json<CreateBody>,
) -> AppResult<Json<Bookmark>> {
    if body.timestamp_seconds < 0 {
        return Err(AppError::BadRequest(
            "timestamp_seconds must be non-negative".into(),
        ));
    }

    let row: Bookmark = sqlx::query_as(
        "INSERT INTO video_bookmarks \
            (child_account_id, video_id, video_title, timestamp_seconds, label) \
         VALUES (?, ?, ?, ?, ?) \
         ON CONFLICT(child_account_id, video_id, timestamp_seconds) DO UPDATE SET \
            label = COALESCE(excluded.label, video_bookmarks.label), \
            video_title = COALESCE(excluded.video_title, video_bookmarks.video_title) \
         RETURNING id, video_id, video_title, timestamp_seconds, label, created_at",
    )
    .bind(current.id)
    .bind(&body.video_id)
    .bind(&body.video_title)
    .bind(body.timestamp_seconds)
    .bind(&body.label)
    .fetch_one(&state.db)
    .await?;
    Ok(Json(row))
}

#[derive(Debug, Deserialize)]
pub struct UpdateBody {
    pub label: Option<String>,
}

/// `PUT /api/bookmarks/:id`.
pub async fn update(
    State(state): State<AppState>,
    current: CurrentAccount,
    Path(id): Path<i64>,
    Json(body): Json<UpdateBody>,
) -> AppResult<Json<Bookmark>> {
    require_owner(&state, current.id, id).await?;
    let row: Bookmark = sqlx::query_as(
        "UPDATE video_bookmarks SET label = ? WHERE id = ? \
         RETURNING id, video_id, video_title, timestamp_seconds, label, created_at",
    )
    .bind(&body.label)
    .bind(id)
    .fetch_one(&state.db)
    .await?;
    Ok(Json(row))
}

/// `DELETE /api/bookmarks/:id`.
pub async fn delete(
    State(state): State<AppState>,
    current: CurrentAccount,
    Path(id): Path<i64>,
) -> AppResult<StatusCode> {
    require_owner(&state, current.id, id).await?;
    sqlx::query("DELETE FROM video_bookmarks WHERE id = ?")
        .bind(id)
        .execute(&state.db)
        .await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn require_owner(state: &AppState, child_id: i64, bookmark_id: i64) -> AppResult<()> {
    let count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM video_bookmarks WHERE id = ? AND child_account_id = ?",
    )
    .bind(bookmark_id)
    .bind(child_id)
    .fetch_one(&state.db)
    .await?;
    if count == 0 {
        Err(AppError::NotFound)
    } else {
        Ok(())
    }
}
