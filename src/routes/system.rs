//! System / yt-dlp API (parent only).
//!
//! - `GET /api/system/ytdlp` — version info from the `ytdlp_info`
//!   singleton plus the latest known GitHub tag (refreshed in the
//!   background if older than 1 hour).
//! - `POST /api/system/ytdlp/update` — kick off the update job
//!   immediately by triggering the matching `cron_jobs` row.

use std::sync::atomic::{AtomicI64, Ordering};

use axum::{extract::State, Json};
use chrono::Utc;
use serde::Serialize;

use crate::error::{AppError, AppResult};
use crate::services::cron::NAME_YTDLP_UPDATE;
use crate::services::ytdlp;
use crate::state::AppState;

/// Cached "latest known" version + timestamp. Populated lazily on first
/// `/api/system/ytdlp` hit; refreshed whenever older than 1 hour.
static LATEST_VERSION_FETCHED_AT: AtomicI64 = AtomicI64::new(0);
static LATEST_VERSION_LOCK: tokio::sync::Mutex<Option<String>> =
    tokio::sync::Mutex::const_new(None);

const REFRESH_INTERVAL_SECONDS: i64 = 3600;

#[derive(Debug, Serialize)]
pub struct YtdlpStatus {
    pub current_version: Option<String>,
    pub latest_known_version: Option<String>,
    pub last_checked_at: Option<i64>,
    pub last_updated_at: Option<i64>,
    pub binary_path: String,
}

/// Tuple shape returned by the `ytdlp_info` SELECT in [`get_ytdlp`].
type YtdlpInfoRow = (Option<String>, Option<i64>, Option<i64>, String);

/// `GET /api/system/ytdlp`.
pub async fn get_ytdlp(State(state): State<AppState>) -> AppResult<Json<YtdlpStatus>> {
    let row: Option<YtdlpInfoRow> = sqlx::query_as(
        "SELECT current_version, last_checked_at, last_updated_at, binary_path \
         FROM ytdlp_info WHERE id = 1",
    )
    .fetch_optional(&state.db)
    .await?;
    let (current_version, last_checked_at, last_updated_at, binary_path) =
        row.unwrap_or((None, None, None, state.config.ytdlp_path.clone()));

    // Refresh the latest known version if stale.
    let now = Utc::now().timestamp();
    let last_refreshed = LATEST_VERSION_FETCHED_AT.load(Ordering::Relaxed);
    if now - last_refreshed > REFRESH_INTERVAL_SECONDS {
        // Fire-and-forget — never block the request on it.
        let pool = state.db.clone();
        tokio::spawn(async move {
            if let Ok(version) = ytdlp::latest_published_version().await {
                let mut guard = LATEST_VERSION_LOCK.lock().await;
                *guard = Some(version);
                LATEST_VERSION_FETCHED_AT.store(Utc::now().timestamp(), Ordering::Relaxed);
            }
            // Persist the last_checked_at column.
            let _ = sqlx::query("UPDATE ytdlp_info SET last_checked_at = unixepoch() WHERE id = 1")
                .execute(&pool)
                .await;
        });
    }

    let latest_known_version = LATEST_VERSION_LOCK.lock().await.clone();

    Ok(Json(YtdlpStatus {
        current_version,
        latest_known_version,
        last_checked_at,
        last_updated_at,
        binary_path,
    }))
}

#[derive(Debug, Serialize)]
pub struct UpdateResponse {
    pub run_id: i64,
}

/// `POST /api/system/ytdlp/update`.
///
/// Resolves the `ytdlp_update` job from `cron_jobs` and triggers it via
/// the scheduler.
pub async fn update_ytdlp(State(state): State<AppState>) -> AppResult<Json<UpdateResponse>> {
    let sched = state
        .scheduler
        .as_ref()
        .ok_or_else(|| AppError::Other(anyhow::anyhow!("scheduler not initialised")))?;
    let job_id: i64 = sqlx::query_scalar("SELECT id FROM cron_jobs WHERE name = ?")
        .bind(NAME_YTDLP_UPDATE)
        .fetch_optional(&state.db)
        .await?
        .ok_or(AppError::NotFound)?;
    let run_id = sched.run_now(job_id).await?;
    Ok(Json(UpdateResponse { run_id }))
}
