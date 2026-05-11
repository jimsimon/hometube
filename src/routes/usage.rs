//! Usage-tracking routes.
//!
//! The video player POSTs a heartbeat every ~30 seconds while playback
//! is active. The handler:
//!
//! 1. Upserts a row in `usage_log` for the current child + video,
//!    extending the `ended_at`/`duration_seconds` of the active row.
//!    A new row is started on a different video or after a 60-second
//!    gap.
//! 2. Upserts `watch_history` (`progress_seconds`, `duration_seconds`,
//!    `last_watched_at`).
//! 3. Returns the remaining seconds for today plus a flag the player
//!    can use to pause itself when the limit is reached.

use axum::{extract::State, Json};
use chrono::{Datelike, Local};
use serde::{Deserialize, Serialize};

use crate::error::{AppError, AppResult};
use crate::middleware::auth::CurrentAccount;
use crate::models::account::AccountType;
use crate::state::AppState;

/// If the player has been silent for this long, the next heartbeat
/// starts a new `usage_log` row.
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
    /// How long since the last heartbeat (defaults to 30s if omitted).
    #[serde(default)]
    pub elapsed_seconds: Option<i64>,
}

#[derive(Debug, Serialize)]
pub struct HeartbeatResponse {
    pub remaining_seconds: Option<i64>,
    pub limit_exceeded: bool,
}

/// `POST /api/usage/heartbeat`.
pub async fn heartbeat(
    State(state): State<AppState>,
    current: CurrentAccount,
    Json(body): Json<HeartbeatBody>,
) -> AppResult<Json<HeartbeatResponse>> {
    if !matches!(current.account_type, AccountType::Child) {
        return Err(AppError::Forbidden);
    }
    let elapsed = body.elapsed_seconds.unwrap_or(30).clamp(1, 90);

    upsert_usage_log(&state, current.id, &body.video_id, elapsed).await?;
    upsert_watch_history(&state, current.id, &body).await?;

    let dow = Local::now().weekday().num_days_from_sunday() as i64;
    let max_hours: Option<f64> = sqlx::query_scalar(
        "SELECT max_hours FROM usage_limits WHERE child_account_id = ? AND day_of_week = ?",
    )
    .bind(current.id)
    .bind(dow)
    .fetch_optional(&state.db)
    .await?;

    let used_today: i64 = sqlx::query_scalar(
        "SELECT COALESCE(SUM(duration_seconds), 0) FROM usage_log \
         WHERE child_account_id = ? AND started_at >= unixepoch() - 86400",
    )
    .bind(current.id)
    .fetch_one(&state.db)
    .await
    .unwrap_or(0);

    let remaining = max_hours.map(|h| ((h * 3600.0) as i64 - used_today).max(0));
    let limit_exceeded = matches!(remaining, Some(0));

    Ok(Json(HeartbeatResponse {
        remaining_seconds: remaining,
        limit_exceeded,
    }))
}

async fn upsert_usage_log(
    state: &AppState,
    child_id: i64,
    video_id: &str,
    elapsed: i64,
) -> AppResult<()> {
    // Most-recent row for this child.
    let last: Option<(i64, String, i64, Option<i64>, Option<i64>)> = sqlx::query_as(
        "SELECT id, video_id, started_at, ended_at, duration_seconds \
         FROM usage_log \
         WHERE child_account_id = ? \
         ORDER BY id DESC LIMIT 1",
    )
    .bind(child_id)
    .fetch_optional(&state.db)
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
        sqlx::query(
            "UPDATE usage_log SET ended_at = ?, duration_seconds = ? WHERE id = ?",
        )
        .bind(now)
        .bind(dur + elapsed)
        .bind(id)
        .execute(&state.db)
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
        .execute(&state.db)
        .await?;
    }
    Ok(())
}

async fn upsert_watch_history(
    state: &AppState,
    child_id: i64,
    body: &HeartbeatBody,
) -> AppResult<()> {
    sqlx::query(
        "INSERT INTO watch_history \
            (child_account_id, video_id, video_title, video_thumbnail_url, channel_title, \
             duration_seconds, progress_seconds, last_watched_at) \
         VALUES (?, ?, ?, ?, ?, ?, ?, unixepoch()) \
         ON CONFLICT(child_account_id, video_id) DO UPDATE SET \
            progress_seconds = excluded.progress_seconds, \
            duration_seconds = COALESCE(excluded.duration_seconds, watch_history.duration_seconds), \
            video_title = COALESCE(excluded.video_title, watch_history.video_title), \
            video_thumbnail_url = COALESCE(excluded.video_thumbnail_url, watch_history.video_thumbnail_url), \
            channel_title = COALESCE(excluded.channel_title, watch_history.channel_title), \
            last_watched_at = unixepoch()",
    )
    .bind(child_id)
    .bind(&body.video_id)
    .bind(body.video_title.clone().unwrap_or_default())
    .bind(body.video_thumbnail_url.clone())
    .bind(body.channel_title.clone())
    .bind(body.duration_seconds)
    .bind(body.position_seconds)
    .execute(&state.db)
    .await?;
    Ok(())
}
