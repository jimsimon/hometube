//! Usage-tracking routes.
//!
//! The video player POSTs a heartbeat every 30 seconds while playback
//! is active. The handler:
//!
//! 1. Coalesces the heartbeat into a single `usage_log` row per
//!    (child, video) — `started_at` is set on first heartbeat, and
//!    `ended_at` / `duration_seconds` are extended on each subsequent
//!    heartbeat. The row is closed (and a new one started on the next
//!    heartbeat) when the video changes or more than
//!    [`NEW_ROW_GAP_SECONDS`] elapsed since the last heartbeat.
//! 2. Upserts `watch_history` (`progress_seconds`, `duration_seconds`,
//!    `last_watched_at`).
//!
//! The whole operation runs inside a SQLite transaction so we never
//! lose count if two heartbeats race.

use axum::{extract::State, Json};
use serde::Deserialize;

use crate::error::AppResult;
use crate::middleware::auth::CurrentAccount;
use crate::state::AppState;

/// If the player has been silent for this long, the next heartbeat
/// closes the previous `usage_log` row and starts a new one.
const NEW_ROW_GAP_SECONDS: i64 = 60;

#[derive(Debug, Deserialize)]
pub struct HeartbeatBody {
    pub video_id: String,
    pub position_seconds: i64,
    pub duration_seconds: Option<i64>,
    /// Optional metadata for `watch_history` upsert.
    #[serde(default)]
    pub video_title: Option<String>,
    #[serde(default)]
    pub video_thumbnail_url: Option<String>,
    #[serde(default)]
    pub channel_title: Option<String>,
    /// Channel ID for the video, used by continue-watching to run an
    /// access check against `allowlisted_channels`. Optional because
    /// not every player surface (e.g. preview) populates it, and old
    /// clients won't send it at all.
    #[serde(default)]
    pub channel_id: Option<String>,
    /// How long since the last heartbeat (defaults to 30s if omitted).
    #[serde(default)]
    pub elapsed_seconds: Option<i64>,
}

/// `POST /api/usage/heartbeat`.
///
/// The route is gated by `require_child` middleware in
/// [`crate::routes::router`], so we don't re-check the role here.
pub async fn heartbeat(
    State(state): State<AppState>,
    current: CurrentAccount,
    Json(body): Json<HeartbeatBody>,
) -> AppResult<axum::http::StatusCode> {
    let elapsed = body.elapsed_seconds.unwrap_or(30).clamp(1, 90);

    upsert_usage_log(&state, current.id, &body.video_id, elapsed).await?;
    upsert_watch_history(&state, current.id, &body).await?;

    Ok(axum::http::StatusCode::NO_CONTENT)
}

/// Tuple shape of the most-recent `usage_log` row queried in
/// [`upsert_usage_log`]. Columns in order: `id, video_id, started_at,
/// ended_at, duration_seconds`.
type LastUsageLogRow = (i64, String, i64, Option<i64>, Option<i64>);

async fn upsert_usage_log(
    state: &AppState,
    child_id: i64,
    video_id: &str,
    elapsed: i64,
) -> AppResult<()> {
    let mut tx = state.db.begin().await?;

    // Most-recent row for this child.
    let last: Option<LastUsageLogRow> = sqlx::query_as(
        "SELECT id, video_id, started_at, ended_at, duration_seconds \
         FROM usage_log \
         WHERE child_account_id = ? \
         ORDER BY id DESC LIMIT 1",
    )
    .bind(child_id)
    .fetch_optional(&mut *tx)
    .await?;

    let now = chrono::Utc::now().timestamp();

    let extend = match last {
        Some((id, vid, _started, ended, dur)) => {
            let last_ended = ended.unwrap_or(now - elapsed);
            if vid == video_id && now - last_ended <= NEW_ROW_GAP_SECONDS {
                Some((id, dur.unwrap_or(0)))
            } else {
                None
            }
        }
        None => None,
    };

    if let Some((id, dur)) = extend {
        sqlx::query("UPDATE usage_log SET ended_at = ?, duration_seconds = ? WHERE id = ?")
            .bind(now)
            .bind(dur + elapsed)
            .bind(id)
            .execute(&mut *tx)
            .await?;
    } else {
        sqlx::query(
            "INSERT INTO usage_log \
                (child_account_id, video_id, started_at, ended_at, duration_seconds) \
             VALUES (?, ?, ?, ?, ?)",
        )
        .bind(child_id)
        .bind(video_id)
        .bind(now)
        .bind(now)
        .bind(elapsed)
        .execute(&mut *tx)
        .await?;
    }

    tx.commit().await?;
    Ok(())
}

#[derive(Debug, Deserialize)]
pub struct ProgressBody {
    pub video_id: String,
    pub position_seconds: i64,
    #[serde(default)]
    pub duration_seconds: Option<i64>,
    #[serde(default)]
    pub video_title: Option<String>,
    #[serde(default)]
    pub video_thumbnail_url: Option<String>,
    #[serde(default)]
    pub channel_title: Option<String>,
    #[serde(default)]
    pub channel_id: Option<String>,
}

/// `POST /api/usage/progress`.
///
/// Position-only update — writes to `watch_history` so the resume
/// point is accurate to 1s, but does **not** touch `usage_log`. The
/// player fires this on pause, seek, and ended; the 30s heartbeat
/// continues to drive screen-time accounting.
pub async fn progress(
    State(state): State<AppState>,
    current: CurrentAccount,
    Json(body): Json<ProgressBody>,
) -> AppResult<axum::http::StatusCode> {
    // Reuse the same upsert as the heartbeat handler by adapting the
    // payload to `HeartbeatBody`. Keeps the SQL in exactly one place.
    // Clamp untrusted client input. A negative position or duration
    // would render as nonsense in continue-watching.
    let position = body.position_seconds.max(0);
    let duration = body.duration_seconds.map(|d| d.max(0));
    let hb = HeartbeatBody {
        video_id: body.video_id,
        position_seconds: position,
        duration_seconds: duration,
        video_title: body.video_title,
        video_thumbnail_url: body.video_thumbnail_url,
        channel_title: body.channel_title,
        channel_id: body.channel_id,
        elapsed_seconds: None,
    };
    upsert_watch_history(&state, current.id, &hb).await?;
    Ok(axum::http::StatusCode::NO_CONTENT)
}

async fn upsert_watch_history(
    state: &AppState,
    child_id: i64,
    body: &HeartbeatBody,
) -> AppResult<()> {
    // `watch_history.video_title` is `NOT NULL`, so the INSERT path
    // must always have a value — fall back to empty string when the
    // caller didn't provide one (e.g. the progress endpoint). On the
    // UPDATE path we bind the raw Option so `COALESCE` keeps the
    // previously-stored title instead of clobbering it with "".
    let title_insert = body.video_title.clone().unwrap_or_default();
    let title_update = body.video_title.clone();
    sqlx::query(
        "INSERT INTO watch_history \
            (child_account_id, video_id, video_title, video_thumbnail_url, channel_title, \
             channel_id, duration_seconds, progress_seconds, last_watched_at) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, unixepoch()) \
         ON CONFLICT(child_account_id, video_id) DO UPDATE SET \
            progress_seconds = excluded.progress_seconds, \
            duration_seconds = COALESCE(excluded.duration_seconds, watch_history.duration_seconds), \
            video_title = COALESCE(?, watch_history.video_title), \
            video_thumbnail_url = COALESCE(excluded.video_thumbnail_url, watch_history.video_thumbnail_url), \
            channel_title = COALESCE(excluded.channel_title, watch_history.channel_title), \
            channel_id = COALESCE(excluded.channel_id, watch_history.channel_id), \
            last_watched_at = unixepoch()",
    )
    .bind(child_id)
    .bind(&body.video_id)
    .bind(title_insert)
    .bind(body.video_thumbnail_url.clone())
    .bind(body.channel_title.clone())
    .bind(body.channel_id.clone())
    .bind(body.duration_seconds)
    .bind(body.position_seconds)
    .bind(title_update)
    .execute(&state.db)
    .await?;
    Ok(())
}
