//! Two-layer video-metadata cache.
//!
//! Layer 1 — **In-memory**: tokio `Mutex<HashMap<video_id, CachedMetadata>>`,
//! 4-hour TTL by default. Survives the process; cleared on restart.
//!
//! Layer 2 — **`video_metadata_cache` table**: persists across restarts.
//! TTL is configurable via the `metadata_cache_ttl_hours` key in
//! `app_config` (default 4 hours).
//!
//! See [`get_or_extract`] for the lookup order: memory → DB → yt-dlp.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use chrono::Utc;
use sqlx::SqlitePool;
use tokio::sync::Mutex;
use tracing::debug;

use crate::config::Config;
use crate::error::{AppError, AppResult};
use crate::services::setup::{get_config_value, set_config_value};
use crate::services::ytdlp::{self, ExtractResult};

/// `app_config` key controlling the metadata-cache TTL (in hours).
pub const KEY_METADATA_CACHE_TTL_HOURS: &str = "metadata_cache_ttl_hours";

/// Default TTL when the key is unset.
pub const DEFAULT_TTL_HOURS: i64 = 4;

#[derive(Clone)]
pub struct CachedMetadata {
    pub fetched_at: Instant,
    pub result: ExtractResult,
}

/// Process-wide cache handle. Cheap to clone (`Arc<Mutex<...>>`).
#[derive(Clone, Default)]
pub struct VideoCache {
    inner: Arc<Mutex<HashMap<String, CachedMetadata>>>,
}

impl VideoCache {
    pub fn new() -> Self {
        Self::default()
    }

    /// Look up `video_id` in memory → DB → yt-dlp. Stores the result in
    /// both layers on the way out.
    pub async fn get_or_extract(
        &self,
        pool: &SqlitePool,
        cfg: &Config,
        video_id: &str,
    ) -> AppResult<ExtractResult> {
        let ttl = current_ttl(pool).await;

        // Layer 1: in-memory.
        {
            let cache = self.inner.lock().await;
            if let Some(entry) = cache.get(video_id) {
                if entry.fetched_at.elapsed() < ttl {
                    debug!(%video_id, "video metadata in-memory cache hit");
                    return Ok(entry.result.clone());
                }
            }
        }

        // Layer 2: DB.
        if let Some(result) = load_from_db(pool, video_id).await? {
            let mut cache = self.inner.lock().await;
            cache.insert(
                video_id.to_string(),
                CachedMetadata {
                    fetched_at: Instant::now(),
                    result: result.clone(),
                },
            );
            debug!(%video_id, "video metadata DB cache hit");
            return Ok(result);
        }

        // Miss: shell out to yt-dlp.
        let result = ytdlp::extract(cfg, video_id).await?;
        store_in_db(pool, video_id, &result, ttl).await?;
        let mut cache = self.inner.lock().await;
        cache.insert(
            video_id.to_string(),
            CachedMetadata {
                fetched_at: Instant::now(),
                result: result.clone(),
            },
        );
        Ok(result)
    }
}

/// Resolve the configured TTL — falls back to [`DEFAULT_TTL_HOURS`] on
/// any error.
async fn current_ttl(pool: &SqlitePool) -> Duration {
    let hours = get_config_value(pool, KEY_METADATA_CACHE_TTL_HOURS)
        .await
        .ok()
        .flatten()
        .and_then(|s| s.parse::<i64>().ok())
        .unwrap_or(DEFAULT_TTL_HOURS);
    Duration::from_secs((hours.max(0) as u64) * 3600)
}

async fn load_from_db(pool: &SqlitePool, video_id: &str) -> AppResult<Option<ExtractResult>> {
    let row: Option<(String, i64)> = sqlx::query_as(
        "SELECT metadata_json, expires_at FROM video_metadata_cache WHERE video_id = ?",
    )
    .bind(video_id)
    .fetch_optional(pool)
    .await?;
    let Some((json, expires_at)) = row else {
        return Ok(None);
    };
    if expires_at <= Utc::now().timestamp() {
        return Ok(None);
    }
    let result: ExtractResult = serde_json::from_str(&json)
        .map_err(|e| AppError::Other(anyhow::anyhow!("decoding cached metadata: {e}")))?;
    Ok(Some(result))
}

async fn store_in_db(
    pool: &SqlitePool,
    video_id: &str,
    result: &ExtractResult,
    ttl: Duration,
) -> AppResult<()> {
    let json = serde_json::to_string(result)
        .map_err(|e| AppError::Other(anyhow::anyhow!("encoding metadata: {e}")))?;
    let now = Utc::now().timestamp();
    let expires_at = now + ttl.as_secs() as i64;
    sqlx::query(
        "INSERT INTO video_metadata_cache (video_id, metadata_json, cached_at, expires_at) \
         VALUES (?, ?, ?, ?) \
         ON CONFLICT(video_id) DO UPDATE SET \
            metadata_json = excluded.metadata_json, \
            cached_at = excluded.cached_at, \
            expires_at = excluded.expires_at",
    )
    .bind(video_id)
    .bind(json)
    .bind(now)
    .bind(expires_at)
    .execute(pool)
    .await?;
    Ok(())
}

/// Persist the metadata-cache TTL to `app_config`. Used by the parent
/// settings UI.
#[allow(dead_code)]
pub async fn set_ttl_hours(pool: &SqlitePool, hours: i64) -> AppResult<()> {
    set_config_value(pool, KEY_METADATA_CACHE_TTL_HOURS, &hours.to_string()).await
}
