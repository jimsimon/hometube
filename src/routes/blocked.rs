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
    // `video_title` is hydrated from the shared `videos` table
    // (migration 024).
    let rows: Vec<BlockedVideo> = sqlx::query_as(
        "SELECT bv.id, bv.video_id, v.title AS video_title, bv.reason, bv.created_at \
         FROM blocked_videos bv \
         JOIN videos v ON v.video_id = bv.video_id \
         WHERE bv.child_account_id = ? \
         ORDER BY bv.created_at DESC",
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

    // Normalize once and reuse for every lookup/insert below so we never
    // persist or query with surrounding whitespace (a padded id would
    // silently fail to match real playback / access checks). Reject a
    // blank id before touching the DB — an empty id would seed a junk
    // `videos` row (PK = "") and a `blocked_videos` row no real video
    // can ever match.
    let video_id = body.video_id.trim();
    if video_id.is_empty() {
        return Err(AppError::BadRequest("video_id must not be empty".into()));
    }

    // Best-effort metadata lookup so the parent UI shows the title.
    // The `blocked_videos` schema doesn't store a thumbnail (just title
    // + reason), so we don't bother with the thumbnail URL here.
    let title = match YoutubeClient::from_db(&state.db).await {
        Ok(yt) => yt.get_video(video_id).await.ok().flatten().map(|v| v.title),
        Err(_) => None,
    };
    // Treat a blank sidecar title as absent. `models::video::upsert`'s
    // INSERT path binds the title as-is (no `NULLIF`), so a `Some("")`
    // would persist an empty title on first sighting instead of the
    // `video_id` placeholder; every other caller filters empties to
    // `None` to uphold that contract.
    let title = title.filter(|s| !s.trim().is_empty());

    let mut tx = state.db.begin().await?;
    // Seed `videos` first so the FK on `blocked_videos.video_id` is
    // satisfied. `None` falls back to the video_id at INSERT time and
    // leaves any pre-existing richer title untouched on CONFLICT.
    crate::models::video::upsert(&mut *tx, video_id, title.as_deref(), None, None, None).await?;
    sqlx::query(
        "INSERT INTO blocked_videos (child_account_id, video_id, blocked_by, reason) \
         VALUES (?, ?, ?, ?) \
         ON CONFLICT(child_account_id, video_id) DO UPDATE SET reason = excluded.reason",
    )
    .bind(child_id)
    .bind(video_id)
    .bind(current.id)
    .bind(body.reason.clone())
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;

    let row: BlockedVideo = sqlx::query_as(
        "SELECT bv.id, bv.video_id, v.title AS video_title, bv.reason, bv.created_at \
         FROM blocked_videos bv \
         JOIN videos v ON v.video_id = bv.video_id \
         WHERE bv.child_account_id = ? AND bv.video_id = ?",
    )
    .bind(child_id)
    .bind(video_id)
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
