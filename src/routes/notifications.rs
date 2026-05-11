//! Parent-notification routes (parent-only).
//!
//! Backed by the `parent_notifications` table. The dispatcher service
//! at [`crate::services::notifications`] writes; this module reads /
//! mutates state via mark-read and dismiss endpoints.
//!
//! Routes:
//! - `GET /api/notifications?limit=&before=`
//! - `GET /api/notifications/unread-count`
//! - `PUT /api/notifications/:id/read`
//! - `PUT /api/notifications/read-all`
//! - `DELETE /api/notifications/:id`

use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    Json,
};
use serde::{Deserialize, Serialize};

use crate::error::{AppError, AppResult};
use crate::middleware::auth::CurrentAccount;
use crate::state::AppState;

const DEFAULT_LIMIT: i64 = 50;
const MAX_LIMIT: i64 = 200;

#[derive(Debug, Serialize, sqlx::FromRow)]
pub struct Notification {
    pub id: i64,
    pub notification_type: String,
    pub title: String,
    pub message: String,
    pub metadata: Option<String>,
    pub is_read: i64,
    pub created_at: i64,
}

#[derive(Debug, Deserialize)]
pub struct ListQuery {
    #[serde(default)]
    pub limit: Option<i64>,
    /// Pagination cursor — return notifications with id < `before`.
    #[serde(default)]
    pub before: Option<i64>,
}

/// `GET /api/notifications` — unread first, then most-recent first.
///
/// "Unread first" is implemented by a primary `ORDER BY is_read ASC`
/// before the secondary chronological sort.
pub async fn list(
    State(state): State<AppState>,
    current: CurrentAccount,
    Query(q): Query<ListQuery>,
) -> AppResult<Json<Vec<Notification>>> {
    let limit = q.limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT);
    let before = q.before.unwrap_or(i64::MAX);
    let rows: Vec<Notification> = sqlx::query_as(
        "SELECT id, notification_type, title, message, metadata, is_read, created_at \
         FROM parent_notifications \
         WHERE parent_account_id = ? AND id < ? \
         ORDER BY is_read ASC, created_at DESC, id DESC \
         LIMIT ?",
    )
    .bind(current.id)
    .bind(before)
    .bind(limit)
    .fetch_all(&state.db)
    .await?;
    Ok(Json(rows))
}

#[derive(Debug, Serialize)]
pub struct UnreadCount {
    pub unread: i64,
}

/// `GET /api/notifications/unread-count`.
pub async fn unread_count(
    State(state): State<AppState>,
    current: CurrentAccount,
) -> AppResult<Json<UnreadCount>> {
    let unread: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM parent_notifications \
         WHERE parent_account_id = ? AND is_read = 0",
    )
    .bind(current.id)
    .fetch_one(&state.db)
    .await?;
    Ok(Json(UnreadCount { unread }))
}

/// `PUT /api/notifications/:id/read`.
pub async fn mark_read(
    State(state): State<AppState>,
    current: CurrentAccount,
    Path(id): Path<i64>,
) -> AppResult<StatusCode> {
    let res = sqlx::query(
        "UPDATE parent_notifications SET is_read = 1 \
         WHERE id = ? AND parent_account_id = ?",
    )
    .bind(id)
    .bind(current.id)
    .execute(&state.db)
    .await?;
    if res.rows_affected() == 0 {
        return Err(AppError::NotFound);
    }
    Ok(StatusCode::NO_CONTENT)
}

/// `PUT /api/notifications/read-all`.
pub async fn mark_all_read(
    State(state): State<AppState>,
    current: CurrentAccount,
) -> AppResult<StatusCode> {
    sqlx::query(
        "UPDATE parent_notifications SET is_read = 1 \
         WHERE parent_account_id = ? AND is_read = 0",
    )
    .bind(current.id)
    .execute(&state.db)
    .await?;
    Ok(StatusCode::NO_CONTENT)
}

/// `DELETE /api/notifications/:id`.
pub async fn delete(
    State(state): State<AppState>,
    current: CurrentAccount,
    Path(id): Path<i64>,
) -> AppResult<StatusCode> {
    let res = sqlx::query(
        "DELETE FROM parent_notifications \
         WHERE id = ? AND parent_account_id = ?",
    )
    .bind(id)
    .bind(current.id)
    .execute(&state.db)
    .await?;
    if res.rows_affected() == 0 {
        return Err(AppError::NotFound);
    }
    Ok(StatusCode::NO_CONTENT)
}
