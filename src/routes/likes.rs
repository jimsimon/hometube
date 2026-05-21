//! Child like routes.
//!
//! Likes are mirrored locally in `video_likes`. The local UI updates
//! immediately.

use axum::{
    extract::{Path, State},
    http::StatusCode,
    Json,
};
use serde::{Deserialize, Serialize};

use crate::error::{AppError, AppResult};
use crate::middleware::auth::CurrentAccount;
use crate::state::AppState;

/// Optional metadata supplied by the client on `POST /api/likes/:videoId`.
///
/// The player already has the video's title and thumbnail in scope when
/// the like button is clicked (it must — the player rendered the video
/// before the button could be pressed), so we let the client send what
/// it has rather than re-fetching from YouTube. Both fields are
/// optional so:
/// - A re-like after a soft-unlike (`is_deleted = 1`) doesn't need to
///   resend metadata; the existing row's values are preserved via
///   `COALESCE` in the upsert below.
/// - A child who somehow likes a video without the player context
///   (a future deep-link, an offline replay) still succeeds with a
///   metadata-less row rather than failing.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct LikeBody {
    pub title: Option<String>,
    pub thumbnail_url: Option<String>,
    /// Channel the video belongs to. Captured at like-time so the
    /// `visible` flag in [`LikeRow`] can also match against
    /// `allowlisted_channels` without re-fetching yt-dlp metadata.
    pub channel_id: Option<String>,
    pub channel_title: Option<String>,
}

#[derive(Debug, Serialize, sqlx::FromRow)]
pub struct LikeRow {
    pub id: i64,
    pub video_id: String,
    pub video_title: Option<String>,
    pub video_thumbnail_url: Option<String>,
    pub channel_id: Option<String>,
    pub channel_title: Option<String>,
    pub liked_at: i64,
    /// `true` when the liked video is reachable through the child's
    /// allowlist. Matches `can_child_view` for the video-allowlist and
    /// channel-allowlist paths (the latter via the `channel_id`
    /// captured at like-time). Playlist-allowlist matches are not
    /// considered — `video_likes` doesn't track playlist membership.
    pub visible: bool,
}

type LikeRowTuple = (
    i64,
    String,
    Option<String>,
    Option<String>,
    Option<String>,
    Option<String>,
    i64,
    i64,
);

/// Shared SELECT projection used by `list` and `like`. Computes
/// `visible` from a direct-video allowlist match OR an allowlisted
/// channel match against the per-like `channel_id`.
const LIKE_ROW_SELECT: &str = "SELECT l.id, l.video_id, l.video_title, l.video_thumbnail_url, \
            l.channel_id, l.channel_title, l.liked_at, \
            CASE WHEN a.id IS NOT NULL OR c.id IS NOT NULL THEN 1 ELSE 0 END AS visible \
     FROM video_likes l \
     LEFT JOIN allowlisted_videos a \
       ON a.child_account_id = l.child_account_id AND a.video_id = l.video_id \
     LEFT JOIN allowlisted_channels c \
       ON c.child_account_id = l.child_account_id \
      AND l.channel_id IS NOT NULL AND c.channel_id = l.channel_id";

fn row_from_tuple(tuple: LikeRowTuple) -> LikeRow {
    let (
        id,
        video_id,
        video_title,
        video_thumbnail_url,
        channel_id,
        channel_title,
        liked_at,
        visible,
    ) = tuple;
    LikeRow {
        id,
        video_id,
        video_title,
        video_thumbnail_url,
        channel_id,
        channel_title,
        liked_at,
        visible: visible != 0,
    }
}

/// `GET /api/likes`.
///
/// Returns liked videos with a `visible` flag derived from a JOIN
/// against `allowlisted_videos`. Likes for videos the parent hasn't
/// allowlisted are returned with `visible: false` so the child UI can
/// filter them out.
pub async fn list(
    State(state): State<AppState>,
    current: CurrentAccount,
) -> AppResult<Json<Vec<LikeRow>>> {
    let sql = format!(
        "{LIKE_ROW_SELECT} \
         WHERE l.child_account_id = ? AND l.is_deleted = 0 \
         ORDER BY visible DESC, l.liked_at DESC"
    );
    let rows: Vec<LikeRowTuple> = sqlx::query_as(&sql)
        .bind(current.id)
        .fetch_all(&state.db)
        .await?;
    let out = rows.into_iter().map(row_from_tuple).collect();
    Ok(Json(out))
}

/// `POST /api/likes/:videoId`.
///
/// Accepts an optional JSON body with `title` and `thumbnail_url` from
/// the player (which already has them in scope) so we don't fan out to
/// the discovery sidecar on every like. Missing fields don't fail the
/// request — the row gets `NULL` columns and the upsert's `COALESCE`
/// preserves any previously-stored metadata on re-like.
pub async fn like(
    State(state): State<AppState>,
    current: CurrentAccount,
    Path(video_id): Path<String>,
    body: Option<Json<LikeBody>>,
) -> AppResult<Json<LikeRow>> {
    let LikeBody {
        title,
        thumbnail_url: thumb,
        channel_id,
        channel_title,
    } = body.map(|Json(b)| b).unwrap_or_default();
    // Treat empty strings as absent so the upsert's `COALESCE` keeps any
    // previously-stored value rather than overwriting it with "".
    let title = title.filter(|s| !s.trim().is_empty());
    let thumb = thumb.filter(|s| !s.trim().is_empty());
    let channel_id = channel_id.filter(|s| !s.trim().is_empty());
    let channel_title = channel_title.filter(|s| !s.trim().is_empty());

    sqlx::query(
        "INSERT INTO video_likes \
            (child_account_id, video_id, video_title, video_thumbnail_url, \
             channel_id, channel_title, is_deleted) \
         VALUES (?, ?, ?, ?, ?, ?, 0) \
         ON CONFLICT(child_account_id, video_id) DO UPDATE SET \
            video_title = COALESCE(excluded.video_title, video_likes.video_title), \
            video_thumbnail_url = COALESCE(excluded.video_thumbnail_url, video_likes.video_thumbnail_url), \
            channel_id = COALESCE(excluded.channel_id, video_likes.channel_id), \
            channel_title = COALESCE(excluded.channel_title, video_likes.channel_title), \
            is_deleted = 0, \
            updated_at = unixepoch()",
    )
    .bind(current.id)
    .bind(&video_id)
    .bind(&title)
    .bind(&thumb)
    .bind(&channel_id)
    .bind(&channel_title)
    .execute(&state.db)
    .await?;

    let sql = format!("{LIKE_ROW_SELECT} WHERE l.child_account_id = ? AND l.video_id = ?");
    let row: LikeRowTuple = sqlx::query_as(&sql)
        .bind(current.id)
        .bind(&video_id)
        .fetch_one(&state.db)
        .await?;
    Ok(Json(row_from_tuple(row)))
}

/// `DELETE /api/likes/:videoId`.
pub async fn unlike(
    State(state): State<AppState>,
    current: CurrentAccount,
    Path(video_id): Path<String>,
) -> AppResult<StatusCode> {
    let result = sqlx::query(
        "UPDATE video_likes \
         SET is_deleted = 1, updated_at = unixepoch() \
         WHERE child_account_id = ? AND video_id = ?",
    )
    .bind(current.id)
    .bind(&video_id)
    .execute(&state.db)
    .await?;
    if result.rows_affected() == 0 {
        return Err(AppError::NotFound);
    }
    Ok(StatusCode::NO_CONTENT)
}
