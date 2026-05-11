//! Usage-tracking routes.
//!
//! The video player POSTs a heartbeat every 30 seconds while playback
//! is active. The handler:
//!
//! 1. Coalesces the heartbeat into a single `usage_log` row per
//!    (child, video) — `started_at` is set on first heartbeat, and
//!    `ended_at` / `duration_seconds` are extended on each subsequent
//!    heartbeat. The row is closed (and a new one started on the next
//!    heartbeat) when (a) the video changes, (b) more than
//!    [`NEW_ROW_GAP_SECONDS`] elapsed since the last heartbeat, or
//!    (c) the response returns `limit_exceeded`.
//! 2. Upserts `watch_history` (`progress_seconds`, `duration_seconds`,
//!    `last_watched_at`).
//! 3. Returns the remaining seconds for today, the allowed window
//!    (HH:MM start/end), and a flag the player can use to pause itself
//!    when the limit is reached.
//!
//! The whole operation runs inside a SQLite transaction so we never
//! lose count if two heartbeats race.

use axum::{extract::State, Json};
use chrono::{Datelike, Local, Timelike};
use serde::{Deserialize, Serialize};

use crate::error::AppResult;
use crate::middleware::auth::CurrentAccount;
use crate::services::notifications;
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
    /// How long since the last heartbeat (defaults to 30s if omitted).
    #[serde(default)]
    pub elapsed_seconds: Option<i64>,
}

#[derive(Debug, Serialize)]
pub struct AllowedWindow {
    pub start: String,
    pub end: String,
}

#[derive(Debug, Serialize)]
pub struct HeartbeatResponse {
    /// Seconds left in the daily cap, or `None` if no limit configured.
    pub remaining_seconds: Option<i64>,
    /// Today's allowed window (HH:MM), or `None` if no limit configured.
    pub allowed_window: Option<AllowedWindow>,
    pub limit_exceeded: bool,
    /// Populated when the player should stop. One of `"limit_exceeded"`
    /// or `"outside_window"`. Mirrors what the usage-limit middleware
    /// returns from a 403 response so the client can use the same code
    /// path either way.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<&'static str>,
}

/// `POST /api/usage/heartbeat`.
///
/// The route is gated by `require_child` middleware in
/// [`crate::routes::router`], so we don't re-check the role here.
pub async fn heartbeat(
    State(state): State<AppState>,
    current: CurrentAccount,
    Json(body): Json<HeartbeatBody>,
) -> AppResult<Json<HeartbeatResponse>> {
    let elapsed = body.elapsed_seconds.unwrap_or(30).clamp(1, 90);

    upsert_usage_log(&state, current.id, &body.video_id, elapsed).await?;
    upsert_watch_history(&state, current.id, &body).await?;

    // Compute today's quota, used time, and allowed window in one pass.
    let now = Local::now();
    let dow = now.weekday().num_days_from_sunday() as i64;
    let limit_row: Option<(f64, String, String)> = sqlx::query_as(
        "SELECT max_hours, allowed_start_time, allowed_end_time \
         FROM usage_limits \
         WHERE child_account_id = ? AND day_of_week = ?",
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

    let mut remaining: Option<i64> = None;
    let mut allowed_window: Option<AllowedWindow> = None;
    let mut reason: Option<&'static str> = None;
    let now_minutes = now.hour() as i64 * 60 + now.minute() as i64;

    if let Some((max_hours, start, end)) = limit_row {
        let max_seconds = (max_hours * 3600.0) as i64;
        remaining = Some((max_seconds - used_today).max(0));
        allowed_window = Some(AllowedWindow {
            start: start.clone(),
            end: end.clone(),
        });
        if let (Some(start_m), Some(end_m)) = (parse_hhmm(&start), parse_hhmm(&end)) {
            if now_minutes < start_m || now_minutes >= end_m {
                reason = Some("outside_window");
            }
        }
        if matches!(remaining, Some(0)) {
            reason = Some("limit_exceeded");
        }
    }

    let limit_exceeded = reason.is_some();

    if limit_exceeded {
        // Force-close the in-flight `usage_log` row when the server is
        // about to tell the player to pause.
        let _ = close_open_log(&state, current.id, &body.video_id).await;
    }

    if reason == Some("limit_exceeded") {
        let _ = ensure_limit_reached_notification(&state, current.id).await;
    }

    Ok(Json(HeartbeatResponse {
        remaining_seconds: remaining,
        allowed_window,
        limit_exceeded,
        reason,
    }))
}

fn parse_hhmm(s: &str) -> Option<i64> {
    let bytes = s.as_bytes();
    if bytes.len() != 5 || bytes[2] != b':' {
        return None;
    }
    let hh: i64 = s[..2].parse().ok()?;
    let mm: i64 = s[3..].parse().ok()?;
    Some(hh * 60 + mm)
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

/// Definitively close the in-flight log row for `(child, video)` so the
/// next heartbeat starts fresh. Used when the server signals
/// `limit_exceeded` mid-session.
async fn close_open_log(state: &AppState, child_id: i64, video_id: &str) -> AppResult<()> {
    let now = chrono::Utc::now().timestamp();
    sqlx::query(
        "UPDATE usage_log \
         SET ended_at = ? \
         WHERE id = ( \
            SELECT id FROM usage_log \
              WHERE child_account_id = ? AND video_id = ? \
              ORDER BY id DESC LIMIT 1 \
         )",
    )
    .bind(now)
    .bind(child_id)
    .bind(video_id)
    .execute(&state.db)
    .await?;
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

/// Insert a `time_limit_reached` notification for every parent unless
/// one already exists for today. Idempotent within a calendar day —
/// dedup is handled by [`crate::services::notifications`].
async fn ensure_limit_reached_notification(state: &AppState, child_id: i64) -> AppResult<()> {
    let metadata = serde_json::json!({ "child_account_id": child_id });
    let key = notifications::json_fragment_key("child_account_id", &child_id);
    notifications::broadcast_once_per_day(
        &state.db,
        notifications::TYPE_TIME_LIMIT_REACHED,
        &key,
        "Daily limit reached",
        "Watch time used up for today.",
        &metadata,
    )
    .await
}
