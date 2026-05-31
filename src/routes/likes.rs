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
    /// Video length in seconds. Captured at like-time from the player
    /// so the `/child/liked` grid can render a duration badge without
    /// re-fetching yt-dlp metadata.
    pub duration_seconds: Option<i64>,
}

#[derive(Debug, Serialize, sqlx::FromRow)]
pub struct LikeRow {
    pub id: i64,
    pub video_id: String,
    pub video_title: Option<String>,
    pub video_thumbnail_url: Option<String>,
    pub channel_id: Option<String>,
    pub channel_title: Option<String>,
    pub duration_seconds: Option<i64>,
    pub liked_at: i64,
    /// Source publish date as unix seconds, joined from
    /// `channel_videos`. `None` when the liked video has no archive row
    /// (e.g. liked straight from search and never seen in a feed).
    pub published_at: Option<i64>,
    /// `true` when the child can currently play the liked video.
    /// Matches the SQL-expressible portion of `can_child_view`: the
    /// video must be allowlisted (directly or via the captured
    /// `channel_id`) AND not blocked AND not in this child's hidden
    /// list. Playlist-allowlist matches are not considered —
    /// `video_likes` doesn't track playlist membership; a like that is
    /// reachable purely via an allowlisted playlist returns
    /// `visible: false`.
    pub visible: bool,
}

type LikeRowTuple = (
    i64,
    String,
    Option<String>,
    Option<String>,
    Option<String>,
    Option<String>,
    Option<i64>,
    i64,
    Option<i64>,
    i64,
);

// Both queries below share the same SELECT + JOINs; `concat!` requires
// string literals, so the projection is repeated inline rather than
// hoisted into a `const`. `visible` mirrors the SQL-expressible portion
// of [`crate::services::access::can_child_view`]: a like is visible iff
// it is in `allowlisted_videos` OR its captured `channel_id` is in
// `allowlisted_channels`, AND it is not blocked, AND it is not in this
// child's `hidden_videos`. Playlist-allowlist matches are not
// considered (`video_likes` doesn't track playlist membership) — same
// caveat documented on [`LikeRow::visible`].

// Metadata is hydrated from the shared `videos` + `channels` tables
// (migrations 024 + 025). The visibility CASE uses `v.channel_id`
// — which the upserts on POST keep current — instead of a denormalised
// per-like column.
const LIKE_LIST_SQL: &str = concat!(
    "SELECT l.id, l.video_id, v.title AS video_title, \
            v.thumbnail_url AS video_thumbnail_url, \
            v.channel_id, ch.channel_title, v.duration_seconds, l.liked_at, \
            vpa.published_at, \
            CASE WHEN (a.id IS NOT NULL OR c.id IS NOT NULL) \
                  AND b.id IS NULL AND h.id IS NULL \
                 THEN 1 ELSE 0 END AS visible \
     FROM video_likes l \
     JOIN videos v ON v.video_id = l.video_id \
     LEFT JOIN channels ch ON ch.channel_id = v.channel_id \
     LEFT JOIN video_published_at vpa ON vpa.video_id = v.video_id \
     LEFT JOIN allowlisted_videos a \
       ON a.child_account_id = l.child_account_id AND a.video_id = l.video_id \
     LEFT JOIN allowlisted_channels c \
       ON c.child_account_id = l.child_account_id \
      AND v.channel_id IS NOT NULL AND c.channel_id = v.channel_id \
     LEFT JOIN blocked_videos b \
       ON b.child_account_id = l.child_account_id AND b.video_id = l.video_id \
     LEFT JOIN hidden_videos h \
       ON h.child_account_id = l.child_account_id AND h.video_id = l.video_id",
    " WHERE l.child_account_id = ? AND l.is_deleted = 0 \
       ORDER BY visible DESC, l.liked_at DESC",
);

const LIKE_ONE_SQL: &str = concat!(
    "SELECT l.id, l.video_id, v.title AS video_title, \
            v.thumbnail_url AS video_thumbnail_url, \
            v.channel_id, ch.channel_title, v.duration_seconds, l.liked_at, \
            vpa.published_at, \
            CASE WHEN (a.id IS NOT NULL OR c.id IS NOT NULL) \
                  AND b.id IS NULL AND h.id IS NULL \
                 THEN 1 ELSE 0 END AS visible \
     FROM video_likes l \
     JOIN videos v ON v.video_id = l.video_id \
     LEFT JOIN channels ch ON ch.channel_id = v.channel_id \
     LEFT JOIN video_published_at vpa ON vpa.video_id = v.video_id \
     LEFT JOIN allowlisted_videos a \
       ON a.child_account_id = l.child_account_id AND a.video_id = l.video_id \
     LEFT JOIN allowlisted_channels c \
       ON c.child_account_id = l.child_account_id \
      AND v.channel_id IS NOT NULL AND c.channel_id = v.channel_id \
     LEFT JOIN blocked_videos b \
       ON b.child_account_id = l.child_account_id AND b.video_id = l.video_id \
     LEFT JOIN hidden_videos h \
       ON h.child_account_id = l.child_account_id AND h.video_id = l.video_id",
    " WHERE l.child_account_id = ? AND l.video_id = ?",
);

fn row_from_tuple(tuple: LikeRowTuple) -> LikeRow {
    let (
        id,
        video_id,
        video_title,
        video_thumbnail_url,
        channel_id,
        channel_title,
        duration_seconds,
        liked_at,
        published_at,
        visible,
    ) = tuple;
    LikeRow {
        id,
        video_id,
        video_title,
        video_thumbnail_url,
        channel_id,
        channel_title,
        duration_seconds,
        liked_at,
        published_at,
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
    let rows: Vec<LikeRowTuple> = sqlx::query_as(LIKE_LIST_SQL)
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
        duration_seconds,
    } = body.map(|Json(b)| b).unwrap_or_default();
    // Treat empty strings as absent so the upsert's `COALESCE` keeps any
    // previously-stored value rather than overwriting it with "".
    let title = title.filter(|s| !s.trim().is_empty());
    let thumb = thumb.filter(|s| !s.trim().is_empty());
    let channel_id = channel_id.filter(|s| !s.trim().is_empty());
    let channel_title = channel_title.filter(|s| !s.trim().is_empty());
    // A zero-or-negative duration is meaningless; treat as absent so a
    // subsequent re-like with the real value isn't blocked by COALESCE.
    let duration_seconds = duration_seconds.filter(|d| *d > 0);

    // Seed `videos` (and `channels` when the client supplied channel
    // metadata) so the JOINs in `LIKE_ONE_SQL` resolve. Empty strings
    // are treated as missing — upsert keeps any previously-stored
    // values via its NULLIF guards.
    let mut tx = state.db.begin().await?;
    if let Some(cid) = channel_id.as_deref() {
        crate::services::feed_cache::upsert_channel_with_metadata(
            &mut *tx,
            cid,
            channel_title.as_deref(),
            None,
            None,
        )
        .await?;
    }
    crate::models::video::upsert(
        &mut *tx,
        &video_id,
        title.as_deref(),
        channel_id.as_deref(),
        duration_seconds,
        thumb.as_deref(),
    )
    .await?;
    sqlx::query(
        "INSERT INTO video_likes (child_account_id, video_id, is_deleted) \
         VALUES (?, ?, 0) \
         ON CONFLICT(child_account_id, video_id) DO UPDATE SET \
            is_deleted = 0, updated_at = unixepoch()",
    )
    .bind(current.id)
    .bind(&video_id)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;

    let row: LikeRowTuple = sqlx::query_as(LIKE_ONE_SQL)
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
