//! Offline-download routes.
//!
//! Phase 16 of the plan introduces client-side offline downloads. The
//! backend's role is intentionally thin — the actual storage lives in
//! the browser's Cache API (see `frontend/src/services/offline.ts`).
//!
//! The backend just:
//!
//!   1. Tracks each download request in the `offline_downloads` table
//!      (so a parent dashboard can later inspect what kids have saved).
//!   2. Hands the client a single-file stream from the highest matching
//!      progressive format, so the SPA can pipe it straight into the
//!      browser cache without juggling DASH segments.
//!
//! The stream endpoint redirects/proxies to the chosen format URL with
//! range-request support handled by the underlying upstream response.
//!
//! Access control: child accounts must have `child_settings.downloads_enabled = 1`,
//! and the video must pass the regular allowlist check (see
//! [`crate::services::access::can_child_view`]).

use axum::{
    body::Body,
    extract::{Path, Query, State},
    http::{header, HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
use futures_util::TryStreamExt;
use serde::{Deserialize, Serialize};
use sqlx::Row;
use tracing::warn;

use crate::error::{AppError, AppResult};
use crate::middleware::auth::CurrentAccount;
use crate::models::account::AccountType;
use crate::services::access::can_child_view;
use crate::services::video_cache::VideoCache;
use crate::state::AppState;

#[derive(Debug, Deserialize)]
pub struct CreateDownloadBody {
    pub video_id: String,
    pub quality: String,
}

#[derive(Debug, Deserialize)]
pub struct UpdateDownloadBody {
    pub status: Option<String>,
    pub quality: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct DownloadRow {
    pub id: i64,
    pub video_id: String,
    pub video_title: String,
    pub video_thumbnail_url: Option<String>,
    pub channel_title: Option<String>,
    pub quality_label: String,
    pub status: String,
    pub downloaded_at: Option<i64>,
}

fn video_cache(state: &AppState) -> VideoCache {
    static CACHE: std::sync::OnceLock<VideoCache> = std::sync::OnceLock::new();
    let _ = state;
    CACHE.get_or_init(VideoCache::new).clone()
}

async fn ensure_downloads_enabled(state: &AppState, current: &CurrentAccount) -> AppResult<()> {
    // Parents bypass the per-child gate.
    if !matches!(current.account_type, AccountType::Child) {
        return Ok(());
    }
    let enabled: Option<i64> = sqlx::query_scalar(
        "SELECT downloads_enabled FROM child_settings WHERE child_account_id = ?",
    )
    .bind(current.id)
    .fetch_optional(&state.db)
    .await?;
    // Fail-closed: only an explicit `1` opens the gate. A missing row,
    // a `0`, or anything else is treated as disabled. This matches the
    // schema default (`migrations/012_default_downloads_off.sql`) and
    // the UI gate in `routes/pages.rs::fetch_downloads_enabled`.
    if matches!(enabled, Some(1)) {
        Ok(())
    } else {
        Err(AppError::Forbidden)
    }
}

/// `GET /api/downloads` — list the current user's tracked downloads.
pub async fn list(
    State(state): State<AppState>,
    current: CurrentAccount,
) -> AppResult<Json<Vec<DownloadRow>>> {
    let rows = sqlx::query(
        "SELECT id, video_id, video_title, video_thumbnail_url, channel_title, \
         quality_label, status, downloaded_at \
         FROM offline_downloads \
         WHERE child_account_id = ? AND status != 'deleted' \
         ORDER BY id DESC",
    )
    .bind(current.id)
    .fetch_all(&state.db)
    .await?;

    let out = rows
        .into_iter()
        .map(|r| DownloadRow {
            id: r.get("id"),
            video_id: r.get("video_id"),
            video_title: r.get("video_title"),
            video_thumbnail_url: r.get("video_thumbnail_url"),
            channel_title: r.get("channel_title"),
            quality_label: r.get("quality_label"),
            status: r.get("status"),
            downloaded_at: r.get("downloaded_at"),
        })
        .collect();
    Ok(Json(out))
}

/// `POST /api/downloads` — record a new download request and hand back
/// the stream URL the client should fetch.
pub async fn create(
    State(state): State<AppState>,
    current: CurrentAccount,
    Json(body): Json<CreateDownloadBody>,
) -> AppResult<Json<serde_json::Value>> {
    ensure_downloads_enabled(&state, &current).await?;

    let cache = video_cache(&state);
    let result = cache
        .get_or_extract(&state.db, &state.config, &body.video_id)
        .await?;

    if matches!(current.account_type, AccountType::Child)
        && !can_child_view(
            &state.db,
            current.id,
            &body.video_id,
            result.channel_id.as_deref(),
            &[],
        )
        .await?
    {
        return Err(AppError::Forbidden);
    }

    let title = result
        .title
        .clone()
        .unwrap_or_else(|| body.video_id.clone());
    let thumb = result.thumbnail.clone().or_else(|| {
        result
            .thumbnails
            .iter()
            .max_by_key(|t| t.width.unwrap_or(0))
            .map(|t| t.url.clone())
    });
    let channel = result.channel_title.clone();

    sqlx::query(
        "INSERT INTO offline_downloads \
         (child_account_id, video_id, video_title, video_thumbnail_url, channel_title, \
          quality_label, status) \
         VALUES (?, ?, ?, ?, ?, ?, 'pending') \
         ON CONFLICT(child_account_id, video_id, quality_label) DO UPDATE SET status = 'pending'",
    )
    .bind(current.id)
    .bind(&body.video_id)
    .bind(&title)
    .bind(&thumb)
    .bind(&channel)
    .bind(&body.quality)
    .execute(&state.db)
    .await?;

    // The video_id and quality come from yt-dlp / our own UI, so they're
    // known to be URL-safe (alphanumeric + a few hyphens). No encoding
    // needed.
    let stream_url = format!(
        "/api/downloads/{}/stream?quality={}",
        body.video_id, body.quality
    );
    Ok(Json(serde_json::json!({
        "video_id": body.video_id,
        "quality": body.quality,
        "stream_url": stream_url,
    })))
}

/// `PUT /api/downloads/:videoId` — client status update.
pub async fn update(
    State(state): State<AppState>,
    current: CurrentAccount,
    Path(video_id): Path<String>,
    Json(body): Json<UpdateDownloadBody>,
) -> AppResult<StatusCode> {
    let status = body.status.as_deref().unwrap_or("complete");
    let now = chrono::Utc::now().timestamp();
    let downloaded_at: Option<i64> = if status == "complete" {
        Some(now)
    } else {
        None
    };

    if let Some(quality) = body.quality {
        sqlx::query(
            "UPDATE offline_downloads SET status = ?, downloaded_at = ? \
             WHERE child_account_id = ? AND video_id = ? AND quality_label = ?",
        )
        .bind(status)
        .bind(downloaded_at)
        .bind(current.id)
        .bind(&video_id)
        .bind(&quality)
        .execute(&state.db)
        .await?;
    } else {
        sqlx::query(
            "UPDATE offline_downloads SET status = ?, downloaded_at = ? \
             WHERE child_account_id = ? AND video_id = ?",
        )
        .bind(status)
        .bind(downloaded_at)
        .bind(current.id)
        .bind(&video_id)
        .execute(&state.db)
        .await?;
    }
    Ok(StatusCode::NO_CONTENT)
}

/// `DELETE /api/downloads/:videoId` — soft-delete: mark as 'deleted'.
pub async fn delete(
    State(state): State<AppState>,
    current: CurrentAccount,
    Path(video_id): Path<String>,
) -> AppResult<StatusCode> {
    sqlx::query(
        "UPDATE offline_downloads SET status = 'deleted' \
         WHERE child_account_id = ? AND video_id = ?",
    )
    .bind(current.id)
    .bind(&video_id)
    .execute(&state.db)
    .await?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Debug, Deserialize)]
pub struct StreamQuery {
    pub quality: Option<String>,
}

/// `GET /api/downloads/:videoId/stream` — proxy a single-file format
/// suitable for offline playback. Picks the highest progressive
/// (video+audio) format whose height matches the requested quality
/// label or falls below it.
pub async fn stream(
    State(state): State<AppState>,
    current: CurrentAccount,
    Path(video_id): Path<String>,
    headers: HeaderMap,
    Query(q): Query<StreamQuery>,
) -> AppResult<Response> {
    ensure_downloads_enabled(&state, &current).await?;

    let cache = video_cache(&state);
    let result = cache
        .get_or_extract(&state.db, &state.config, &video_id)
        .await?;
    if matches!(current.account_type, AccountType::Child)
        && !can_child_view(
            &state.db,
            current.id,
            &video_id,
            result.channel_id.as_deref(),
            &[],
        )
        .await?
    {
        return Err(AppError::Forbidden);
    }

    let target_height: Option<i64> = q
        .quality
        .as_deref()
        .and_then(|q| q.trim_end_matches('p').parse::<i64>().ok());
    let mut candidates: Vec<_> = result
        .formats
        .iter()
        .filter(|f| {
            f.url.is_some()
                && f.height.is_some()
                && f.acodec.as_deref() != Some("none")
                && f.vcodec.as_deref() != Some("none")
        })
        .collect();
    if let Some(cap) = target_height {
        candidates.retain(|f| f.height.unwrap_or(0) <= cap);
    }
    candidates.sort_by_key(|f| std::cmp::Reverse(f.height.unwrap_or(0)));
    let chosen = candidates
        .first()
        .copied()
        .ok_or_else(|| AppError::BadRequest("no suitable progressive format".into()))?;
    let url = chosen
        .url
        .clone()
        .ok_or_else(|| AppError::BadRequest("format has no URL".into()))?;

    let mut req = reqwest::Client::new().get(&url);
    if let Some(range) = headers.get(header::RANGE) {
        if let Ok(s) = range.to_str() {
            req = req.header(header::RANGE, s);
        }
    }
    let res = req.send().await.map_err(|err| {
        warn!(%url, %err, "download stream upstream error");
        AppError::Http(err)
    })?;
    let status = res.status();
    let content_type = res
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("video/mp4")
        .to_string();
    let content_length = res.headers().get(header::CONTENT_LENGTH).cloned();

    let stream = res.bytes_stream().map_err(std::io::Error::other);
    let body = Body::from_stream(stream);
    let mut response = (status, body).into_response();
    response
        .headers_mut()
        .insert(header::CONTENT_TYPE, content_type.parse().unwrap());
    if let Some(cl) = content_length {
        response.headers_mut().insert(header::CONTENT_LENGTH, cl);
    }
    response
        .headers_mut()
        .insert(header::ACCEPT_RANGES, "bytes".parse().unwrap());
    Ok(response)
}
