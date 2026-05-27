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

    // Order matters: `upsert_watch_history` seeds the canonical
    // `videos` row (and, when channel metadata is supplied, the
    // `channels` row) that `top_channels` joins against via INNER JOIN
    // (`routes/usage.rs::top_channels`). Running it *before*
    // `upsert_usage_log` ensures that if the second write fails for
    // any reason, we don't strand a `usage_log` row whose video has no
    // `videos` parent — such a row is silently dropped from the
    // top-channels totals by the INNER JOIN.
    //
    // The two writes are still in separate transactions; folding them
    // into one would mean a single failure in either path loses both
    // the screen-time accounting AND the resume point, which is worse
    // UX than the current "best-effort, watch_history wins on failure".
    //
    // Note also the *converse* failure mode: if `upsert_watch_history`
    // itself fails (e.g. transient lock), the `?` short-circuits and
    // neither write happens — this tick loses both the resume point
    // and the screen-time accounting. Acceptable because both writes
    // are idempotent and the next heartbeat (≤elapsed seconds later,
    // 30s default) recovers both: `upsert_watch_history` re-asserts
    // the same `progress_seconds = excluded.progress_seconds` and
    // `upsert_usage_log` extends the same session window with the
    // newer tick. We surface the failure to the client (500) so a
    // persistent failure isn't silent.
    //
    // INNER JOIN consequence: `top_channels` reads
    // `usage_log u INNER JOIN videos v ON u.video_id = v.video_id`
    // and groups by `v.channel_id IS NOT NULL`. A heartbeat that
    // arrives with `body.channel_id == None` still seeds the `videos`
    // row (with channel_id NULL until a future writer fills it),
    // which means the row is *currently* excluded from
    // `top_channels`. This is documented as intentional in migration
    // 024: rows with no resolvable channel are excluded until a
    // future writer enriches them, so the totals are eventually
    // correct without ever attributing usage to "channel unknown."
    upsert_watch_history(&state, current.id, &body).await?;
    upsert_usage_log(&state, current.id, &body.video_id, elapsed).await?;

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
    // Heartbeats are the *authoritative* refresh path for `videos`:
    // the player has the live title/thumbnail/duration in scope, so we
    // upsert them on every tick. `crate::models::video::upsert` uses
    // NULLIF on the conflict path so an empty string from the body
    // doesn't clobber a previously-stored value.
    //
    // WRITE-AMPLIFICATION TRADE-OFF: this used to be a single
    // `INSERT … ON CONFLICT DO UPDATE` against `watch_history`.
    // Migrations 024/025 split per-video metadata across `videos` +
    // `channels`, so each heartbeat now executes up to three writes
    // (channels upsert, videos upsert, watch_history upsert) under a
    // single transaction. Heartbeats fire roughly every few seconds
    // during playback, so the per-tick cost matters — but on WAL
    // SQLite the three statements share one fsync at commit time and
    // none of them touch indexed columns beyond the PKs, so in
    // practice the cost is dominated by the existing watch_history
    // write. Keeping the upserts here (rather than deferring to a
    // background reconciler) preserves the contract that listing
    // surfaces see the freshest title/thumbnail/duration immediately
    // after a heartbeat — which is what the player UI relies on.
    let mut tx = state.db.begin().await?;

    let channel_id = body.channel_id.as_deref().filter(|s| !s.is_empty());
    let channel_title = body.channel_title.as_deref().filter(|s| !s.is_empty());
    if let Some(cid) = channel_id {
        crate::services::feed_cache::upsert_channel_with_metadata(
            &mut *tx,
            cid,
            channel_title,
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
        channel_id,
        body.duration_seconds.filter(|d| *d > 0),
        body.video_thumbnail_url
            .as_deref()
            .filter(|s| !s.is_empty()),
    )
    .await?;

    sqlx::query(
        "INSERT INTO watch_history \
            (child_account_id, video_id, progress_seconds, last_watched_at) \
         VALUES (?, ?, ?, unixepoch()) \
         ON CONFLICT(child_account_id, video_id) DO UPDATE SET \
            progress_seconds = excluded.progress_seconds, \
            last_watched_at  = unixepoch()",
    )
    .bind(child_id)
    .bind(&body.video_id)
    .bind(body.position_seconds)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(())
}
