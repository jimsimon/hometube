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
    /// Source publish date as unix seconds, joined from
    /// `channel_videos`. `None` when the video has no archive row.
    pub published_at: Option<i64>,
}

/// `GET /api/hidden`.
pub async fn list(
    State(state): State<AppState>,
    current: CurrentAccount,
) -> AppResult<Json<Vec<HiddenVideo>>> {
    // Hydrate from the shared `videos` + `channels` tables.
    let rows: Vec<HiddenVideo> = sqlx::query_as(
        "SELECT hv.id, hv.video_id, \
                v.title AS video_title, \
                v.channel_id, \
                ch.channel_title, \
                v.thumbnail_url AS video_thumbnail_url, \
                v.duration_seconds, \
                hv.hidden_at, \
                vpa.published_at \
         FROM hidden_videos hv \
         JOIN videos v ON v.video_id = hv.video_id \
         LEFT JOIN channels ch ON ch.channel_id = v.channel_id \
         LEFT JOIN video_published_at vpa ON vpa.video_id = v.video_id \
         WHERE hv.child_account_id = ? \
         ORDER BY hv.hidden_at DESC",
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
    let mut tx = state.db.begin().await?;

    // Seed `channels` if the client supplied channel metadata, so the
    // FK on `videos.channel_id` resolves AND the `channels.channel_title`
    // display field gets populated.
    if let Some(channel_id) = body.channel_id.as_deref().filter(|s| !s.is_empty()) {
        crate::services::feed_cache::upsert_channel_with_metadata(
            &mut *tx,
            channel_id,
            body.channel_title.as_deref().filter(|s| !s.is_empty()),
            None,
            None,
        )
        .await?;
    }

    let title = body.video_title.as_deref().filter(|s| !s.is_empty());
    crate::models::video::upsert(
        &mut *tx,
        &body.video_id,
        title,
        body.channel_id.as_deref().filter(|s| !s.is_empty()),
        body.duration_seconds.filter(|d| *d > 0),
        body.video_thumbnail_url
            .as_deref()
            .filter(|s| !s.is_empty()),
    )
    .await?;
    sqlx::query(
        "INSERT INTO hidden_videos (child_account_id, video_id) \
         VALUES (?, ?) \
         ON CONFLICT(child_account_id, video_id) DO UPDATE SET hidden_at = unixepoch()",
    )
    .bind(current.id)
    .bind(&body.video_id)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;

    let row: HiddenVideo = sqlx::query_as(
        "SELECT hv.id, hv.video_id, v.title AS video_title, v.channel_id, \
                ch.channel_title, v.thumbnail_url AS video_thumbnail_url, \
                v.duration_seconds, hv.hidden_at, \
                vpa.published_at \
         FROM hidden_videos hv \
         JOIN videos v ON v.video_id = hv.video_id \
         LEFT JOIN channels ch ON ch.channel_id = v.channel_id \
         LEFT JOIN video_published_at vpa ON vpa.video_id = v.video_id \
         WHERE hv.child_account_id = ? AND hv.video_id = ?",
    )
    .bind(current.id)
    .bind(&body.video_id)
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
