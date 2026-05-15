//! System / yt-dlp API (parent only).
//!
//! - `GET /api/system/ytdlp` — version info from the `ytdlp_info`
//!   singleton plus the latest known GitHub tag (refreshed in the
//!   background if older than 1 hour).
//! - `POST /api/system/ytdlp/update` — kick off the update job
//!   immediately by triggering the matching `cron_jobs` row.
//! - `GET /api/system/pot-server` — PO token server status.

use std::sync::atomic::{AtomicI64, Ordering};

use axum::{extract::State, Json};
use chrono::Utc;
use serde::{Deserialize, Serialize};

use crate::error::{AppError, AppResult};
use crate::services::cron::NAME_YTDLP_UPDATE;
use crate::services::{setup, ytdlp};
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

// -----------------------------------------------------------------------
// PO token server status
// -----------------------------------------------------------------------

#[derive(Debug, Serialize)]
pub struct PotServerStatus {
    /// Whether the PO token server is reachable.
    pub available: bool,
    /// The URL we're trying to reach.
    pub url: String,
    /// Error message if unavailable.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// Whether the yt-dlp plugin is installed.
    pub plugin_installed: bool,
}

/// `GET /api/system/pot-server` — check PO token server health.
pub async fn get_pot_server_status() -> Json<PotServerStatus> {
    let pot_url =
        std::env::var("POT_SERVER_URL").unwrap_or_else(|_| "http://pot-server:4416".to_string());

    let plugin_dir = std::env::var("YTDLP_PLUGIN_DIR")
        .unwrap_or_else(|_| "/usr/local/share/yt-dlp-plugins".to_string());
    let plugin_installed = std::path::Path::new(&plugin_dir).exists()
        && std::fs::read_dir(&plugin_dir)
            .map(|mut d| d.next().is_some())
            .unwrap_or(false);

    // Ping the pot server's health endpoint (/ping).
    let ping_url = format!("{}/ping", pot_url.trim_end_matches('/'));
    let (available, error) = match reqwest::get(&ping_url).await {
        Ok(resp) if resp.status().is_success() => (true, None),
        Ok(resp) => (
            false,
            Some(format!("server returned status {}", resp.status())),
        ),
        Err(e) => (false, Some(format!("connection failed: {e}"))),
    };

    Json(PotServerStatus {
        available,
        url: pot_url,
        error,
        plugin_installed,
    })
}

// -----------------------------------------------------------------------
// yt-dlp cookies management
// -----------------------------------------------------------------------

#[derive(Debug, Serialize)]
pub struct CookiesStatus {
    /// Whether a cookies file is currently configured.
    pub configured: bool,
    /// Number of lines in the stored cookie content (gives feedback
    /// without exposing raw content).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub line_count: Option<usize>,
}

/// `GET /api/system/ytdlp/cookies` — check whether cookies are configured.
pub async fn get_cookies(State(state): State<AppState>) -> AppResult<Json<CookiesStatus>> {
    let content = setup::get_config_value(&state.db, setup::KEY_YTDLP_COOKIES).await?;
    let (configured, line_count) = match content {
        Some(ref c) if !c.trim().is_empty() => (true, Some(c.lines().count())),
        _ => (false, None),
    };
    Ok(Json(CookiesStatus {
        configured,
        line_count,
    }))
}

#[derive(Debug, Deserialize)]
pub struct SetCookiesRequest {
    pub content: String,
}

/// `PUT /api/system/ytdlp/cookies` — store cookie content and sync to disk.
pub async fn set_cookies(
    State(state): State<AppState>,
    Json(body): Json<SetCookiesRequest>,
) -> AppResult<Json<CookiesStatus>> {
    let content = body.content.trim_start().to_string();
    if content.trim().is_empty() {
        return Err(AppError::BadRequest(
            "Cookie content cannot be empty".into(),
        ));
    }

    setup::set_config_value(&state.db, setup::KEY_YTDLP_COOKIES, &content).await?;

    let to_write = content.clone();
    tokio::task::spawn_blocking(move || ytdlp::sync_cookies_to_disk(Some(&to_write)))
        .await
        .map_err(|e| AppError::Other(anyhow::anyhow!("sync task panicked: {e}")))?
        .map_err(|e| AppError::Other(anyhow::anyhow!("failed to write cookies file: {e}")))?;

    let line_count = content.lines().count();
    Ok(Json(CookiesStatus {
        configured: true,
        line_count: Some(line_count),
    }))
}

/// `DELETE /api/system/ytdlp/cookies` — remove stored cookies from DB and disk.
pub async fn delete_cookies(State(state): State<AppState>) -> AppResult<Json<CookiesStatus>> {
    sqlx::query("DELETE FROM app_config WHERE key = ?")
        .bind(setup::KEY_YTDLP_COOKIES)
        .execute(&state.db)
        .await?;

    tokio::task::spawn_blocking(|| ytdlp::sync_cookies_to_disk(None))
        .await
        .map_err(|e| AppError::Other(anyhow::anyhow!("sync task panicked: {e}")))?
        .map_err(|e| AppError::Other(anyhow::anyhow!("failed to remove cookies file: {e}")))?;

    Ok(Json(CookiesStatus {
        configured: false,
        line_count: None,
    }))
}
