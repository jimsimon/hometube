//! Allowlist management routes (parent only).
//!
//! Two flavours: channels and individual videos. Each follows the same
//! shape:
//!
//! - `GET    /api/children/:id/allowlist/{kind}`
//! - `POST   /api/children/:id/allowlist/{kind}`           (body: `{ channel_id|video_id }`)
//! - `DELETE /api/children/:id/allowlist/{kind}/:itemId`
//!
//! The `:id` path parameter must refer to a *child* account; parent IDs
//! are rejected with `400 Bad Request`. Metadata (title, thumbnail) is
//! fetched from YouTube (via the discovery sidecar) at insert time so
//! the UI doesn't have to re-resolve names every time it lists the
//! allowlist.

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

// ---------------------------------------------------------------------------
// Channels
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize, sqlx::FromRow)]
pub struct AllowlistedChannel {
    pub id: i64,
    pub channel_id: String,
    pub channel_title: String,
    pub channel_thumbnail_url: Option<String>,
    pub created_at: i64,
}

#[derive(Debug, Deserialize)]
pub struct AddChannelBody {
    pub channel_id: String,
}

/// `GET /api/children/:id/allowlist/channels`.
pub async fn list_channels(
    State(state): State<AppState>,
    Path(child_id): Path<i64>,
) -> AppResult<Json<Vec<AllowlistedChannel>>> {
    require_child_id(&state, child_id).await?;
    let rows: Vec<AllowlistedChannel> = sqlx::query_as(
        "SELECT id, channel_id, channel_title, channel_thumbnail_url, created_at \
         FROM allowlisted_channels WHERE child_account_id = ? ORDER BY created_at DESC",
    )
    .bind(child_id)
    .fetch_all(&state.db)
    .await?;
    Ok(Json(rows))
}

/// `POST /api/children/:id/allowlist/channels`.
pub async fn add_channel(
    State(state): State<AppState>,
    current: CurrentAccount,
    Path(child_id): Path<i64>,
    Json(body): Json<AddChannelBody>,
) -> AppResult<Json<AllowlistedChannel>> {
    require_child_id(&state, child_id).await?;
    let yt = YoutubeClient::from_db(&state.db).await?;
    let info = yt
        .get_channel(&body.channel_id)
        .await?
        .ok_or_else(|| AppError::BadRequest("channel not found on YouTube".into()))?;
    let thumb = preferred_thumbnail(&info.thumbnails);

    let row: AllowlistedChannel = sqlx::query_as(
        "INSERT INTO allowlisted_channels \
            (child_account_id, channel_id, channel_title, channel_thumbnail_url, added_by) \
         VALUES (?, ?, ?, ?, ?) \
         ON CONFLICT(child_account_id, channel_id) DO UPDATE SET \
            channel_title = excluded.channel_title, \
            channel_thumbnail_url = excluded.channel_thumbnail_url \
         RETURNING id, channel_id, channel_title, channel_thumbnail_url, created_at",
    )
    .bind(child_id)
    .bind(&info.id)
    .bind(&info.title)
    .bind(thumb)
    .bind(current.id)
    .fetch_one(&state.db)
    .await?;

    // Seed the feed-source cache so this channel will be polled by
    // the background refresher. Failures here are logged but do not
    // fail the allowlist write — the user has already committed.
    if let Err(err) = crate::services::feed_cache::upsert_source(
        &state.db,
        crate::services::feed_cache::KIND_CHANNEL,
        &info.id,
    )
    .await
    {
        tracing::warn!(channel_id = %info.id, %err, "failed to seed feed source for newly allowlisted channel");
    }
    Ok(Json(row))
}

/// `DELETE /api/children/:id/allowlist/channels/:channelId`.
///
/// Performs the allowlist delete and the optional `feed_sources` GC
/// inside a single transaction so an observer can never witness "no
/// child references this channel but feed_sources still holds it" —
/// the diagnostics page and the refresher both see a consistent view.
pub async fn delete_channel(
    State(state): State<AppState>,
    Path((child_id, channel_id)): Path<(i64, String)>,
) -> AppResult<StatusCode> {
    require_child_id(&state, child_id).await?;

    let mut tx = state.db.begin().await?;
    sqlx::query("DELETE FROM allowlisted_channels WHERE child_account_id = ? AND channel_id = ?")
        .bind(child_id)
        .bind(&channel_id)
        .execute(&mut *tx)
        .await?;

    // If no other child still has this channel allowlisted, drop the
    // matching `feed_sources` row so the refresher stops polling it
    // immediately rather than waiting up to a day for the `feed_gc`
    // cron. The `feed_source_items` rows cascade via FK.
    let still_used: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM allowlisted_channels WHERE channel_id = ?")
            .bind(&channel_id)
            .fetch_one(&mut *tx)
            .await?;
    if still_used == 0 {
        sqlx::query("DELETE FROM feed_sources WHERE kind = 'channel' AND source_id = ?")
            .bind(&channel_id)
            .execute(&mut *tx)
            .await?;
    }
    tx.commit().await?;
    Ok(StatusCode::NO_CONTENT)
}

// ---------------------------------------------------------------------------
// Videos
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize, sqlx::FromRow)]
pub struct AllowlistedVideo {
    pub id: i64,
    pub video_id: String,
    pub video_title: String,
    pub video_thumbnail_url: Option<String>,
    pub channel_title: Option<String>,
    pub created_at: i64,
}

/// Body for `POST /api/children/:id/allowlist/videos`.
///
/// `video_id` is required; the rest are caller-supplied metadata used
/// **as a fallback** when the discovery sidecar fails to resolve the
/// video. The parent-side allowlist UI already has these fields from
/// the parent search response and passes them through so the row in
/// `allowlisted_videos` always has a non-empty `video_title` (the
/// column the child-side `/api/search` query matches on).
///
/// Sidecar data wins when present and non-empty — the sidecar tends
/// to have canonical, normalised titles. Body data only fills in
/// blanks (e.g. when youtubei.js returns `title: ""`, the video is
/// age-gated, or the network is down).
#[derive(Debug, Deserialize)]
pub struct AddVideoBody {
    pub video_id: String,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub channel_title: Option<String>,
    #[serde(default)]
    pub thumbnail_url: Option<String>,
}

/// `GET /api/children/:id/allowlist/videos`.
pub async fn list_videos(
    State(state): State<AppState>,
    Path(child_id): Path<i64>,
) -> AppResult<Json<Vec<AllowlistedVideo>>> {
    require_child_id(&state, child_id).await?;
    let rows: Vec<AllowlistedVideo> = sqlx::query_as(
        "SELECT id, video_id, video_title, video_thumbnail_url, channel_title, created_at \
         FROM allowlisted_videos WHERE child_account_id = ? ORDER BY created_at DESC",
    )
    .bind(child_id)
    .fetch_all(&state.db)
    .await?;
    Ok(Json(rows))
}

/// `POST /api/children/:id/allowlist/videos`.
///
/// Resolves a title / channel / thumbnail for the video by combining
/// (1) the discovery sidecar response and (2) caller-supplied metadata
/// from the body. Both can be missing or partial, but **at least one**
/// must yield a non-empty title — otherwise we'd write a row that the
/// child-side `LIKE` search could never find, which is exactly the
/// bug this endpoint used to ship.
pub async fn add_video(
    State(state): State<AppState>,
    current: CurrentAccount,
    Path(child_id): Path<i64>,
    Json(body): Json<AddVideoBody>,
) -> AppResult<Json<AllowlistedVideo>> {
    require_child_id(&state, child_id).await?;
    let video_id = parse_video_id(&body.video_id);

    // Best-effort sidecar lookup. We deliberately don't propagate
    // sidecar failures — if the caller provided usable metadata we'd
    // rather write a searchable row than 500.
    let yt = YoutubeClient::from_db(&state.db).await?;
    let info = yt.get_video(&video_id).await.ok().flatten();

    // Treat empty strings from the sidecar as "missing" — youtubei.js
    // emits `title: ""` when it can't parse the basic info response.
    let body_title = body
        .title
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let sidecar_title = info
        .as_ref()
        .map(|i| i.title.trim())
        .filter(|s| !s.is_empty());
    let Some(title) = sidecar_title.or(body_title) else {
        return Err(AppError::BadRequest(
            "video not found on YouTube and no title provided".into(),
        ));
    };
    let title = title.to_string();

    // Trim sidecar `channel_title` for consistency with how we treat
    // the sidecar `title` above and the body-supplied `channel_title`
    // below — a whitespace-only value is functionally identical to an
    // empty one and should not be persisted.
    let channel_title = info
        .as_ref()
        .and_then(|i| i.channel_title.as_ref().map(|s| s.trim().to_string()))
        .filter(|s| !s.is_empty())
        .or_else(|| {
            body.channel_title
                .as_ref()
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
        });
    let thumb = info
        .as_ref()
        .and_then(|i| preferred_thumbnail(&i.thumbnails))
        .or_else(|| {
            body.thumbnail_url
                .as_ref()
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
        });
    let canonical_id = info.as_ref().map(|i| i.id.clone()).unwrap_or(video_id);

    let row: AllowlistedVideo = sqlx::query_as(
        "INSERT INTO allowlisted_videos \
            (child_account_id, video_id, video_title, video_thumbnail_url, channel_title, added_by) \
         VALUES (?, ?, ?, ?, ?, ?) \
         ON CONFLICT(child_account_id, video_id) DO UPDATE SET \
            video_title = excluded.video_title, \
            video_thumbnail_url = excluded.video_thumbnail_url, \
            channel_title = excluded.channel_title \
         RETURNING id, video_id, video_title, video_thumbnail_url, channel_title, created_at",
    )
    .bind(child_id)
    .bind(&canonical_id)
    .bind(&title)
    .bind(thumb)
    .bind(channel_title)
    .bind(current.id)
    .fetch_one(&state.db)
    .await?;
    Ok(Json(row))
}

/// `DELETE /api/children/:id/allowlist/videos/:videoId`.
pub async fn delete_video(
    State(state): State<AppState>,
    Path((child_id, video_id)): Path<(i64, String)>,
) -> AppResult<StatusCode> {
    require_child_id(&state, child_id).await?;
    sqlx::query("DELETE FROM allowlisted_videos WHERE child_account_id = ? AND video_id = ?")
        .bind(child_id)
        .bind(video_id)
        .execute(&state.db)
        .await?;
    Ok(StatusCode::NO_CONTENT)
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

async fn require_child_id(state: &AppState, child_id: i64) -> AppResult<()> {
    if !access::is_child_account(&state.db, child_id).await? {
        return Err(AppError::BadRequest("target account is not a child".into()));
    }
    Ok(())
}

/// Pick the highest-resolution thumbnail URL we have. YouTube returns
/// keyed sizes; "maxres" → "high" → "medium" → "default" → "standard".
pub(crate) fn preferred_thumbnail(
    thumbs: &std::collections::HashMap<String, crate::services::youtube::ThumbnailInfo>,
) -> Option<String> {
    for key in ["maxres", "high", "standard", "medium", "default"] {
        if let Some(t) = thumbs.get(key) {
            return Some(t.url.clone());
        }
    }
    None
}

/// Accept either a raw video ID or a YouTube URL and return the bare ID.
fn parse_video_id(input: &str) -> String {
    let trimmed = input.trim();
    // youtu.be/<id>
    if let Some(rest) = trimmed.strip_prefix("https://youtu.be/") {
        return rest
            .split(['?', '&', '/'])
            .next()
            .unwrap_or(rest)
            .to_string();
    }
    if let Some(rest) = trimmed.strip_prefix("http://youtu.be/") {
        return rest
            .split(['?', '&', '/'])
            .next()
            .unwrap_or(rest)
            .to_string();
    }
    // youtube.com/watch?v=<id>
    if trimmed.contains("youtube.com/watch") {
        if let Some(qpos) = trimmed.find('?') {
            for part in trimmed[qpos + 1..].split('&') {
                if let Some(v) = part.strip_prefix("v=") {
                    return v.to_string();
                }
            }
        }
    }
    // youtube.com/embed/<id> or shorts/<id>
    for prefix in [
        "https://www.youtube.com/embed/",
        "https://www.youtube.com/shorts/",
        "https://youtube.com/embed/",
        "https://youtube.com/shorts/",
    ] {
        if let Some(rest) = trimmed.strip_prefix(prefix) {
            return rest
                .split(['?', '&', '/'])
                .next()
                .unwrap_or(rest)
                .to_string();
        }
    }
    trimmed.to_string()
}
