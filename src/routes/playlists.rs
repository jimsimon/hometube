//! Child playlist routes.
//!
//! `child_playlists` stores the child's playlists; rows have `is_own=1`
//! when the child created the playlist in HomeTube and `is_own=0` when
//! the playlist is a YouTube-side playlist that's been "added to
//! library" via the parent's allowlist.

use axum::{
    extract::{Path, State},
    http::StatusCode,
    Json,
};
use serde::{Deserialize, Serialize};

use crate::error::{AppError, AppResult};
use crate::middleware::auth::CurrentAccount;
use crate::services::youtube::YoutubeClient;
use crate::state::AppState;

// ---------------------------------------------------------------------------
// List view
// ---------------------------------------------------------------------------

/// One row in the playlist list view.
#[derive(Debug, Serialize, sqlx::FromRow)]
pub struct PlaylistSummary {
    pub id: i64,
    pub youtube_playlist_id: Option<String>,
    pub title: String,
    pub description: Option<String>,
    pub is_own: bool,
    pub video_count: i64,
    pub created_at: i64,
    pub updated_at: i64,
    /// `true` when the playlist is reachable through the child's
    /// allowlist. Always `true` for `is_own=1` (child-created)
    /// playlists; for library imports the flag is computed by joining
    /// against `allowlisted_playlists` so inbound playlists the parent
    /// never allowlisted stay hidden.
    pub visible: bool,
}

/// Tuple shape produced by the SELECTs that hydrate
/// [`PlaylistSummary`]. Defined as an alias so clippy's
/// `type_complexity` lint doesn't fire on every queried row binding.
///
/// Columns in order:
/// `id, youtube_playlist_id, title, description, is_own,
///  video_count, created_at, updated_at, visible`.
type PlaylistSummaryRow = (
    i64,
    Option<String>,
    String,
    Option<String>,
    i64,
    i64,
    i64,
    i64,
    i64,
);

/// `GET /api/playlists`.
///
/// Returns the child's playlists with a `visible` flag computed at
/// query time. `is_own=1` rows are always visible (the child created
/// them). `is_own=0` rows with a `youtube_playlist_id` — i.e. inbound
/// library imports — are only marked visible when the underlying
/// `youtube_playlist_id` is in `allowlisted_playlists` for this
/// child. Hidden rows are still returned so the parent UI can
/// reason about them, but the child UI is expected to filter them
/// out.
pub async fn list(
    State(state): State<AppState>,
    current: CurrentAccount,
) -> AppResult<Json<Vec<PlaylistSummary>>> {
    let rows: Vec<PlaylistSummaryRow> =
        sqlx::query_as(
            "SELECT p.id, p.youtube_playlist_id, p.title, p.description, p.is_own, \
                    (SELECT COUNT(*) FROM child_playlist_videos v WHERE v.playlist_id = p.id) AS video_count, \
                    p.created_at, p.updated_at, \
                    CASE \
                        WHEN p.is_own = 1 THEN 1 \
                        WHEN p.youtube_playlist_id IS NULL THEN 1 \
                        WHEN EXISTS ( \
                            SELECT 1 FROM allowlisted_playlists al \
                            WHERE al.child_account_id = p.child_account_id \
                              AND al.playlist_id = p.youtube_playlist_id \
                        ) THEN 1 \
                        ELSE 0 \
                    END AS visible \
             FROM child_playlists p \
             WHERE p.child_account_id = ? AND p.is_deleted = 0 \
             ORDER BY visible DESC, p.updated_at DESC",
        )
        .bind(current.id)
        .fetch_all(&state.db)
        .await?;
    let out = rows
        .into_iter()
        .map(
            |(
                id,
                yt_id,
                title,
                description,
                is_own,
                video_count,
                created_at,
                updated_at,
                visible,
            )| {
                PlaylistSummary {
                    id,
                    youtube_playlist_id: yt_id,
                    title,
                    description,
                    is_own: is_own != 0,
                    video_count,
                    created_at,
                    updated_at,
                    visible: visible != 0,
                }
            },
        )
        .collect();
    Ok(Json(out))
}

// ---------------------------------------------------------------------------
// Create
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct CreatePlaylistBody {
    pub title: String,
    #[serde(default)]
    pub description: Option<String>,
}

/// `POST /api/playlists`.
pub async fn create(
    State(state): State<AppState>,
    current: CurrentAccount,
    Json(body): Json<CreatePlaylistBody>,
) -> AppResult<Json<PlaylistSummary>> {
    let title = body.title.trim();
    if title.is_empty() {
        return Err(AppError::BadRequest("title must not be empty".into()));
    }

    let id: i64 = sqlx::query_scalar(
        "INSERT INTO child_playlists \
            (child_account_id, title, description, is_own, is_deleted) \
         VALUES (?, ?, ?, 1, 0) \
         RETURNING id",
    )
    .bind(current.id)
    .bind(title)
    .bind(&body.description)
    .fetch_one(&state.db)
    .await?;

    let row = fetch_summary(&state, id).await?;
    Ok(Json(row))
}

// ---------------------------------------------------------------------------
// Detail (single playlist + its videos)
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize, sqlx::FromRow)]
pub struct PlaylistVideo {
    pub id: i64,
    pub video_id: String,
    pub video_title: String,
    pub video_thumbnail_url: Option<String>,
    pub channel_title: Option<String>,
    pub position: i64,
    pub added_at: i64,
}

#[derive(Debug, Serialize)]
pub struct PlaylistDetail {
    #[serde(flatten)]
    pub summary: PlaylistSummary,
    pub videos: Vec<PlaylistVideo>,
}

/// `GET /api/playlists/:id`.
///
/// For child-created playlists (`is_own=1`) every video is returned
/// as-is. For YouTube-sourced library playlists (`is_own=0` with a
/// `youtube_playlist_id`) the items are filtered through
/// [`crate::services::access::can_child_view`] so videos the parent
/// hasn't allowlisted (e.g., a deleted-from-allowlist channel that
/// still has tracks in the inbound playlist mirror) are dropped from
/// the response.
pub async fn detail(
    State(state): State<AppState>,
    current: CurrentAccount,
    Path(playlist_id): Path<i64>,
) -> AppResult<Json<PlaylistDetail>> {
    require_owner(&state, current.id, playlist_id).await?;
    let summary = fetch_summary(&state, playlist_id).await?;

    // Lazy-refresh: for YouTube-sourced library playlists, re-populate
    // video items from YouTube if the playlist hasn't been refreshed
    // recently. This avoids hammering the discovery sidecar on repeated views.
    const REFRESH_INTERVAL_SECS: i64 = 15 * 60; // 15 minutes
    if !summary.is_own && summary.youtube_playlist_id.is_some() {
        let now = chrono::Utc::now().timestamp();
        let stale = (now - summary.updated_at) >= REFRESH_INTERVAL_SECS;
        if stale {
            if let Some(yt_playlist_id) = summary.youtube_playlist_id.as_deref() {
                if let Ok(yt) = YoutubeClient::from_db(&state.db).await {
                    if populate_playlist_videos(&state, &yt, playlist_id, yt_playlist_id)
                        .await
                        .is_ok()
                    {
                        // Touch updated_at so we don't refresh again immediately.
                        let _ = sqlx::query(
                            "UPDATE child_playlists SET updated_at = unixepoch() WHERE id = ?",
                        )
                        .bind(playlist_id)
                        .execute(&state.db)
                        .await;
                    }
                }
            }
        }
    }

    let videos: Vec<PlaylistVideo> = sqlx::query_as(
        "SELECT id, video_id, video_title, video_thumbnail_url, channel_title, position, added_at \
         FROM child_playlist_videos \
         WHERE playlist_id = ? \
         ORDER BY position",
    )
    .bind(playlist_id)
    .fetch_all(&state.db)
    .await?;
    // Filter inbound YouTube-sourced playlists through access control.
    let videos = if !summary.is_own && summary.youtube_playlist_id.is_some() {
        let yt_id = summary.youtube_playlist_id.clone().unwrap_or_default();
        let mut out = Vec::with_capacity(videos.len());
        for v in videos {
            // We don't have channel_id on the row; pass the playlist's
            // youtube_playlist_id as one of the playlist-IDs the video
            // appears in so the allowlist-by-playlist branch fires.
            let pl_ids = if yt_id.is_empty() {
                vec![]
            } else {
                vec![yt_id.clone()]
            };
            if crate::services::access::can_child_view(
                &state.db,
                current.id,
                &v.video_id,
                None,
                &pl_ids,
            )
            .await
            .unwrap_or(false)
            {
                out.push(v);
            }
        }
        out
    } else {
        videos
    };
    Ok(Json(PlaylistDetail { summary, videos }))
}

// ---------------------------------------------------------------------------
// Update / delete
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct UpdatePlaylistBody {
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub description: Option<Option<String>>,
}

/// `PUT /api/playlists/:id`.
pub async fn update(
    State(state): State<AppState>,
    current: CurrentAccount,
    Path(playlist_id): Path<i64>,
    Json(body): Json<UpdatePlaylistBody>,
) -> AppResult<Json<PlaylistSummary>> {
    require_owner(&state, current.id, playlist_id).await?;
    let is_own = is_own(&state, playlist_id).await?;
    if !is_own {
        return Err(AppError::BadRequest(
            "library playlists are read-only".into(),
        ));
    }

    if let Some(title) = body.title.as_ref() {
        let trimmed = title.trim();
        if trimmed.is_empty() {
            return Err(AppError::BadRequest("title must not be empty".into()));
        }
        sqlx::query(
            "UPDATE child_playlists SET title = ?, updated_at = unixepoch() \
             WHERE id = ?",
        )
        .bind(trimmed)
        .bind(playlist_id)
        .execute(&state.db)
        .await?;
    }
    if let Some(desc) = body.description.as_ref() {
        sqlx::query(
            "UPDATE child_playlists SET description = ?, updated_at = unixepoch() \
             WHERE id = ?",
        )
        .bind(desc.as_ref())
        .bind(playlist_id)
        .execute(&state.db)
        .await?;
    }

    Ok(Json(fetch_summary(&state, playlist_id).await?))
}

/// `DELETE /api/playlists/:id` — soft delete.
pub async fn delete(
    State(state): State<AppState>,
    current: CurrentAccount,
    Path(playlist_id): Path<i64>,
) -> AppResult<StatusCode> {
    require_owner(&state, current.id, playlist_id).await?;

    sqlx::query(
        "UPDATE child_playlists \
         SET is_deleted = 1, \
             updated_at = unixepoch() \
         WHERE id = ?",
    )
    .bind(playlist_id)
    .execute(&state.db)
    .await?;
    Ok(StatusCode::NO_CONTENT)
}

// ---------------------------------------------------------------------------
// Add / remove / reorder videos
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct AddVideoBody {
    pub video_id: String,
}

/// `POST /api/playlists/:id/videos` — append a video to the playlist.
pub async fn add_video(
    State(state): State<AppState>,
    current: CurrentAccount,
    Path(playlist_id): Path<i64>,
    Json(body): Json<AddVideoBody>,
) -> AppResult<Json<PlaylistVideo>> {
    require_owner(&state, current.id, playlist_id).await?;
    if !is_own(&state, playlist_id).await? {
        return Err(AppError::BadRequest(
            "library playlists are read-only".into(),
        ));
    }

    // Resolve a usable title/thumbnail/channel for the row.
    let yt = YoutubeClient::from_db(&state.db).await?;
    let info = yt
        .get_video(&body.video_id)
        .await?
        .ok_or_else(|| AppError::BadRequest("video not found on YouTube".into()))?;
    let thumb = info
        .thumbnails
        .get("maxres")
        .or_else(|| info.thumbnails.get("high"))
        .or_else(|| info.thumbnails.get("standard"))
        .or_else(|| info.thumbnails.get("medium"))
        .or_else(|| info.thumbnails.get("default"))
        .map(|t| t.url.clone());

    // Compute the next free position.
    let next_position: i64 = sqlx::query_scalar(
        "SELECT COALESCE(MAX(position), -1) + 1 FROM child_playlist_videos \
         WHERE playlist_id = ?",
    )
    .bind(playlist_id)
    .fetch_one(&state.db)
    .await?;

    let row: PlaylistVideo = sqlx::query_as(
        "INSERT INTO child_playlist_videos \
            (playlist_id, video_id, video_title, video_thumbnail_url, channel_title, position) \
         VALUES (?, ?, ?, ?, ?, ?) \
         ON CONFLICT(playlist_id, video_id) DO UPDATE SET \
            video_title = excluded.video_title, \
            video_thumbnail_url = excluded.video_thumbnail_url, \
            channel_title = excluded.channel_title \
         RETURNING id, video_id, video_title, video_thumbnail_url, channel_title, position, added_at",
    )
    .bind(playlist_id)
    .bind(&info.id)
    .bind(&info.title)
    .bind(thumb)
    .bind(info.channel_title.clone())
    .bind(next_position)
    .fetch_one(&state.db)
    .await?;

    sqlx::query("UPDATE child_playlists SET updated_at = unixepoch() WHERE id = ?")
        .bind(playlist_id)
        .execute(&state.db)
        .await?;

    Ok(Json(row))
}

/// `DELETE /api/playlists/:id/videos/:videoId`.
pub async fn remove_video(
    State(state): State<AppState>,
    current: CurrentAccount,
    Path((playlist_id, video_id)): Path<(i64, String)>,
) -> AppResult<StatusCode> {
    require_owner(&state, current.id, playlist_id).await?;
    if !is_own(&state, playlist_id).await? {
        return Err(AppError::BadRequest(
            "library playlists are read-only".into(),
        ));
    }
    let result =
        sqlx::query("DELETE FROM child_playlist_videos WHERE playlist_id = ? AND video_id = ?")
            .bind(playlist_id)
            .bind(&video_id)
            .execute(&state.db)
            .await?;
    if result.rows_affected() == 0 {
        return Err(AppError::NotFound);
    }

    sqlx::query("UPDATE child_playlists SET updated_at = unixepoch() WHERE id = ?")
        .bind(playlist_id)
        .execute(&state.db)
        .await?;

    Ok(StatusCode::NO_CONTENT)
}

#[derive(Debug, Deserialize)]
pub struct ReorderBody {
    pub video_ids: Vec<String>,
}

/// `PUT /api/playlists/:id/videos/reorder`.
///
/// Replaces the `position` of every row in a single transaction. The
/// background sync attempts a YouTube reorder via
/// `playlistItems.update`; YouTube refuses reorder on system playlists
/// (LL/WL) — for those we fail with a 400 immediately.
pub async fn reorder_videos(
    State(state): State<AppState>,
    current: CurrentAccount,
    Path(playlist_id): Path<i64>,
    Json(body): Json<ReorderBody>,
) -> AppResult<Json<Vec<PlaylistVideo>>> {
    require_owner(&state, current.id, playlist_id).await?;
    let is_own = is_own(&state, playlist_id).await?;
    if !is_own {
        return Err(AppError::BadRequest(
            "library playlists are read-only".into(),
        ));
    }
    let yt_id: Option<String> =
        sqlx::query_scalar("SELECT youtube_playlist_id FROM child_playlists WHERE id = ?")
            .bind(playlist_id)
            .fetch_optional(&state.db)
            .await?
            .flatten();
    if let Some(id) = yt_id.as_deref() {
        // YouTube refuses reorder on system playlists.
        if id == "LL" || id == "WL" {
            return Err(AppError::BadRequest(
                "reorder not supported on this playlist".into(),
            ));
        }
    }

    // The schema has no UNIQUE constraint on `(playlist_id, position)`,
    // so a single update pass is safe even when positions transiently
    // collide.
    let mut tx = state.db.begin().await?;
    for (position, video_id) in body.video_ids.iter().enumerate() {
        sqlx::query(
            "UPDATE child_playlist_videos SET position = ? \
             WHERE playlist_id = ? AND video_id = ?",
        )
        .bind(position as i64)
        .bind(playlist_id)
        .bind(video_id)
        .execute(&mut *tx)
        .await?;
    }
    sqlx::query("UPDATE child_playlists SET updated_at = unixepoch() WHERE id = ?")
        .bind(playlist_id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;

    let videos: Vec<PlaylistVideo> = sqlx::query_as(
        "SELECT id, video_id, video_title, video_thumbnail_url, channel_title, position, added_at \
         FROM child_playlist_videos WHERE playlist_id = ? ORDER BY position",
    )
    .bind(playlist_id)
    .fetch_all(&state.db)
    .await?;
    Ok(Json(videos))
}

// ---------------------------------------------------------------------------
// Add allowlisted YouTube playlist to library
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct AddLibraryBody {
    pub youtube_playlist_id: String,
}

/// `POST /api/playlists/library` — import a YouTube playlist into the
/// child's library. The playlist must already be allowlisted by the
/// parent; otherwise the request is refused.
pub async fn add_library(
    State(state): State<AppState>,
    current: CurrentAccount,
    Json(body): Json<AddLibraryBody>,
) -> AppResult<Json<PlaylistSummary>> {
    let allowlisted: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM allowlisted_playlists \
         WHERE child_account_id = ? AND playlist_id = ?",
    )
    .bind(current.id)
    .bind(&body.youtube_playlist_id)
    .fetch_one(&state.db)
    .await?;
    if allowlisted == 0 {
        return Err(AppError::Forbidden);
    }

    let yt = YoutubeClient::from_db(&state.db).await?;
    let info = yt
        .get_playlist(&body.youtube_playlist_id)
        .await?
        .ok_or_else(|| AppError::BadRequest("playlist not found on YouTube".into()))?;

    // Manual duplicate check — `child_playlists` has no UNIQUE
    // constraint on `(child_account_id, youtube_playlist_id)` because
    // app-created playlists may not yet have a YouTube ID. If a soft-
    // deleted row already exists, resurrect it and refresh metadata.
    let existing: Option<(i64, i64)> = sqlx::query_as(
        "SELECT id, is_deleted FROM child_playlists \
         WHERE child_account_id = ? AND youtube_playlist_id = ?",
    )
    .bind(current.id)
    .bind(&info.id)
    .fetch_optional(&state.db)
    .await?;
    if let Some((id, _)) = existing {
        sqlx::query(
            "UPDATE child_playlists SET is_deleted = 0, \
                                           is_own = 0, \
                                           title = ?, description = ?, \
                                           updated_at = unixepoch() \
             WHERE id = ?",
        )
        .bind(&info.title)
        .bind(&info.description)
        .bind(id)
        .execute(&state.db)
        .await?;
        populate_playlist_videos(&state, &yt, id, &body.youtube_playlist_id).await?;
        return Ok(Json(fetch_summary(&state, id).await?));
    }

    let id: i64 = sqlx::query_scalar(
        "INSERT INTO child_playlists \
            (child_account_id, youtube_playlist_id, title, description, is_own, is_deleted) \
         VALUES (?, ?, ?, ?, 0, 0) \
         RETURNING id",
    )
    .bind(current.id)
    .bind(&info.id)
    .bind(&info.title)
    .bind(&info.description)
    .fetch_one(&state.db)
    .await?;
    populate_playlist_videos(&state, &yt, id, &body.youtube_playlist_id).await?;
    Ok(Json(fetch_summary(&state, id).await?))
}

// ---------------------------------------------------------------------------
// Populate playlist videos from YouTube
// ---------------------------------------------------------------------------

/// Fetch all video items from a YouTube playlist and upsert them into
/// `child_playlist_videos`. Used when importing a library playlist and
/// as a lazy-refresh when the child views the playlist detail.
async fn populate_playlist_videos(
    state: &AppState,
    yt: &YoutubeClient,
    playlist_row_id: i64,
    youtube_playlist_id: &str,
) -> AppResult<()> {
    let mut page_token: Option<String> = None;
    let mut idx: usize = 0;
    loop {
        let page = yt
            .list_playlist_items(youtube_playlist_id, 50, page_token.as_deref())
            .await?;
        for item in &page.items {
            let thumb_url = item
                .thumbnails
                .get("maxres")
                .or_else(|| item.thumbnails.get("high"))
                .or_else(|| item.thumbnails.get("standard"))
                .or_else(|| item.thumbnails.get("medium"))
                .or_else(|| item.thumbnails.get("default"))
                .map(|t| t.url.clone());
            let position = item.position.unwrap_or(idx as i64);
            sqlx::query(
                "INSERT INTO child_playlist_videos \
                     (playlist_id, video_id, video_title, video_thumbnail_url, channel_title, position) \
                 VALUES (?, ?, ?, ?, ?, ?) \
                 ON CONFLICT(playlist_id, video_id) DO UPDATE SET \
                     video_title = excluded.video_title, \
                     video_thumbnail_url = excluded.video_thumbnail_url, \
                     channel_title = excluded.channel_title, \
                     position = excluded.position",
            )
            .bind(playlist_row_id)
            .bind(&item.video_id)
            .bind(&item.title)
            .bind(&thumb_url)
            .bind(&item.channel_title)
            .bind(position)
            .execute(&state.db)
            .await?;
            idx += 1;
        }
        match page.next_page_token {
            Some(tok) => page_token = Some(tok),
            None => break,
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

async fn require_owner(state: &AppState, child_id: i64, playlist_id: i64) -> AppResult<()> {
    let count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM child_playlists \
         WHERE id = ? AND child_account_id = ? AND is_deleted = 0",
    )
    .bind(playlist_id)
    .bind(child_id)
    .fetch_one(&state.db)
    .await?;
    if count == 0 {
        Err(AppError::NotFound)
    } else {
        Ok(())
    }
}

async fn is_own(state: &AppState, playlist_id: i64) -> AppResult<bool> {
    let v: i64 = sqlx::query_scalar("SELECT is_own FROM child_playlists WHERE id = ?")
        .bind(playlist_id)
        .fetch_one(&state.db)
        .await?;
    Ok(v != 0)
}

async fn fetch_summary(state: &AppState, playlist_id: i64) -> AppResult<PlaylistSummary> {
    let row: PlaylistSummaryRow =
        sqlx::query_as(
            "SELECT p.id, p.youtube_playlist_id, p.title, p.description, p.is_own, \
                    (SELECT COUNT(*) FROM child_playlist_videos v WHERE v.playlist_id = p.id) AS video_count, \
                    p.created_at, p.updated_at, \
                    CASE \
                        WHEN p.is_own = 1 THEN 1 \
                        WHEN p.youtube_playlist_id IS NULL THEN 1 \
                        WHEN EXISTS ( \
                            SELECT 1 FROM allowlisted_playlists al \
                            WHERE al.child_account_id = p.child_account_id \
                              AND al.playlist_id = p.youtube_playlist_id \
                        ) THEN 1 \
                        ELSE 0 \
                    END AS visible \
             FROM child_playlists p WHERE p.id = ?",
        )
        .bind(playlist_id)
        .fetch_one(&state.db)
        .await?;
    let (id, yt_id, title, description, is_own, video_count, created_at, updated_at, visible) = row;
    Ok(PlaylistSummary {
        id,
        youtube_playlist_id: yt_id,
        title,
        description,
        is_own: is_own != 0,
        video_count,
        created_at,
        updated_at,
        visible: visible != 0,
    })
}
