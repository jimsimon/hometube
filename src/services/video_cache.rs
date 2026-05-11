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
pub async fn set_ttl_hours(pool: &SqlitePool, hours: i64) -> AppResult<()> {
    set_config_value(pool, KEY_METADATA_CACHE_TTL_HOURS, &hours.to_string()).await
}

// ---------------------------------------------------------------------------
// Cache cleanup (Phase 12)
// ---------------------------------------------------------------------------

/// `app_config` key for the cache size cap. Stored as one of the human
/// presets ("5 GB", "10 GB", "25 GB", "50 GB", "100 GB", "Unlimited").
pub const KEY_CACHE_MAX_SIZE: &str = "cache_max_size";
pub const DEFAULT_CACHE_MAX_SIZE: &str = "50 GB";

/// Convert a cache-size preset label to a byte count. `"Unlimited"`
/// returns `0`. Unknown labels also return `0` (treat as unlimited).
pub fn cache_size_preset_to_bytes(label: &str) -> u64 {
    match label.trim() {
        "5 GB" => 5 * 1024 * 1024 * 1024,
        "10 GB" => 10 * 1024 * 1024 * 1024,
        "25 GB" => 25 * 1024 * 1024 * 1024,
        "50 GB" => 50 * 1024 * 1024 * 1024,
        "100 GB" => 100 * 1024 * 1024 * 1024,
        "Unlimited" => 0,
        _ => 0,
    }
}

/// Recognised size presets, in order of presentation.
pub const CACHE_SIZE_PRESETS: &[&str] = &["5 GB", "10 GB", "25 GB", "50 GB", "100 GB", "Unlimited"];

/// Resolve the configured max size, defaulting to [`DEFAULT_CACHE_MAX_SIZE`].
pub async fn current_cache_size_label(pool: &SqlitePool) -> String {
    get_config_value(pool, KEY_CACHE_MAX_SIZE)
        .await
        .ok()
        .flatten()
        .unwrap_or_else(|| DEFAULT_CACHE_MAX_SIZE.to_string())
}

/// Persist the cache-size preset. Validates against [`CACHE_SIZE_PRESETS`].
pub async fn set_cache_size(pool: &SqlitePool, label: &str) -> AppResult<()> {
    if !CACHE_SIZE_PRESETS.contains(&label) {
        return Err(AppError::Other(anyhow::anyhow!(
            "invalid cache size preset: {label}"
        )));
    }
    set_config_value(pool, KEY_CACHE_MAX_SIZE, label).await
}

/// Return `(human-message, detailed-output)` after running the cleanup.
///
/// Step 1 (allowlist cleanup): for every distinct `video_id` in
/// `segment_cache`, drop it if no child has it allowlisted directly,
/// via channel, or via playlist.
///
/// Step 2 (LRU eviction): if a max size is configured and the cache
/// is still over the limit, evict by `last_accessed_at ASC` until under.
pub async fn cleanup_segment_cache(pool: &SqlitePool) -> AppResult<(String, String)> {
    let mut output = String::new();
    let mut evicted_videos: u64 = 0;
    let mut evicted_segments: u64 = 0;
    let mut evicted_bytes: u64 = 0;

    // Allowlist-based cleanup.
    let video_ids: Vec<(String,)> = sqlx::query_as("SELECT DISTINCT video_id FROM segment_cache")
        .fetch_all(pool)
        .await?;

    for (video_id,) in &video_ids {
        let allowlisted = video_is_anywhere_allowlisted(pool, video_id).await?;
        if !allowlisted {
            let (segs, bytes) = evict_video(pool, video_id).await?;
            evicted_videos += 1;
            evicted_segments += segs;
            evicted_bytes += bytes;
            output.push_str(&format!(
                "Evicted {video_id} ({segs} segments, {} KB)\n",
                bytes / 1024
            ));
        }
    }

    // LRU eviction down to the configured size.
    let label = current_cache_size_label(pool).await;
    let limit = cache_size_preset_to_bytes(&label);
    if limit > 0 {
        let mut current_total = total_cache_bytes(pool).await?;
        if current_total > limit {
            output.push_str(&format!(
                "LRU eviction: cache {} bytes > limit {} bytes\n",
                current_total, limit
            ));
            // Pull the LRU-ordered list of (id, bytes, video_id, file_path).
            let rows: Vec<(i64, i64, String, String)> = sqlx::query_as(
                "SELECT id, file_size_bytes, video_id, file_path \
                 FROM segment_cache ORDER BY last_accessed_at ASC",
            )
            .fetch_all(pool)
            .await?;
            for (id, size, video_id, path) in rows {
                if current_total <= limit {
                    break;
                }
                if let Err(err) = tokio::fs::remove_file(&path).await {
                    debug!(%path, %err, "failed to remove segment file");
                }
                sqlx::query("DELETE FROM segment_cache WHERE id = ?")
                    .bind(id)
                    .execute(pool)
                    .await?;
                current_total = current_total.saturating_sub(size as u64);
                evicted_segments += 1;
                evicted_bytes += size as u64;
                let _ = video_id;
            }
        }
    }

    let msg = format!(
        "Cleanup: {evicted_videos} videos / {evicted_segments} segments / {} KB freed.",
        evicted_bytes / 1024
    );
    Ok((msg, output))
}

async fn video_is_anywhere_allowlisted(pool: &SqlitePool, video_id: &str) -> AppResult<bool> {
    // Direct allowlist by video.
    let direct: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM allowlisted_videos WHERE video_id = ?")
            .bind(video_id)
            .fetch_one(pool)
            .await?;
    if direct > 0 {
        return Ok(true);
    }

    // Allowlist via channel: check whether the cached metadata records
    // a channel_id that any child has allowlisted.
    let channel_id: Option<String> =
        sqlx::query_scalar("SELECT metadata_json FROM video_metadata_cache WHERE video_id = ?")
            .bind(video_id)
            .fetch_optional(pool)
            .await?
            .and_then(|json: String| {
                serde_json::from_str::<serde_json::Value>(&json)
                    .ok()
                    .and_then(|v| {
                        v.get("channel_id")
                            .and_then(|c| c.as_str())
                            .map(String::from)
                    })
            });
    if let Some(ch) = channel_id {
        let n: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM allowlisted_channels WHERE channel_id = ?")
                .bind(&ch)
                .fetch_one(pool)
                .await?;
        if n > 0 {
            return Ok(true);
        }
    }

    // Allowlist via playlist: check whether the video appears in any
    // playlist a child has allowlisted (via child_playlist_videos →
    // child_playlists.youtube_playlist_id) — best-effort.
    let n: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM child_playlist_videos cpv \
         INNER JOIN child_playlists cp ON cp.id = cpv.playlist_id \
         INNER JOIN allowlisted_playlists ap ON ap.playlist_id = cp.youtube_playlist_id \
         WHERE cpv.video_id = ?",
    )
    .bind(video_id)
    .fetch_one(pool)
    .await
    .unwrap_or(0);
    Ok(n > 0)
}

async fn evict_video(pool: &SqlitePool, video_id: &str) -> AppResult<(u64, u64)> {
    let rows: Vec<(i64, i64, String)> = sqlx::query_as(
        "SELECT id, file_size_bytes, file_path FROM segment_cache WHERE video_id = ?",
    )
    .bind(video_id)
    .fetch_all(pool)
    .await?;
    let mut bytes_total: u64 = 0;
    let mut segs: u64 = 0;
    for (_id, size, path) in &rows {
        if let Err(err) = tokio::fs::remove_file(path).await {
            debug!(%path, %err, "failed to remove segment file");
        }
        bytes_total += *size as u64;
        segs += 1;
    }
    sqlx::query("DELETE FROM segment_cache WHERE video_id = ?")
        .bind(video_id)
        .execute(pool)
        .await?;
    sqlx::query("DELETE FROM video_metadata_cache WHERE video_id = ?")
        .bind(video_id)
        .execute(pool)
        .await?;
    Ok((segs, bytes_total))
}

/// Total bytes currently in the segment cache.
pub async fn total_cache_bytes(pool: &SqlitePool) -> AppResult<u64> {
    let total: i64 =
        sqlx::query_scalar("SELECT COALESCE(SUM(file_size_bytes), 0) FROM segment_cache")
            .fetch_one(pool)
            .await
            .unwrap_or(0);
    Ok(total.max(0) as u64)
}

/// Total segment count.
pub async fn total_segment_count(pool: &SqlitePool) -> AppResult<i64> {
    let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM segment_cache")
        .fetch_one(pool)
        .await
        .unwrap_or(0);
    Ok(n)
}

/// Per-video aggregate (video_id, total bytes, segment count). Sorted
/// by descending size.
pub async fn list_cached_videos(pool: &SqlitePool) -> AppResult<Vec<(String, i64, i64)>> {
    let rows: Vec<(String, i64, i64)> = sqlx::query_as(
        "SELECT video_id, COALESCE(SUM(file_size_bytes), 0), COUNT(*) \
         FROM segment_cache GROUP BY video_id ORDER BY 2 DESC",
    )
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

/// Manually evict a single video (parent UI).
pub async fn evict_video_public(pool: &SqlitePool, video_id: &str) -> AppResult<(u64, u64)> {
    evict_video(pool, video_id).await
}

/// Wipe the entire segment cache + on-disk files we know about.
pub async fn clear_all(pool: &SqlitePool) -> AppResult<()> {
    let rows: Vec<(String,)> = sqlx::query_as("SELECT file_path FROM segment_cache")
        .fetch_all(pool)
        .await?;
    for (path,) in rows {
        let _ = tokio::fs::remove_file(&path).await;
    }
    sqlx::query("DELETE FROM segment_cache")
        .execute(pool)
        .await?;
    sqlx::query("DELETE FROM video_metadata_cache")
        .execute(pool)
        .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn presets_round_trip_to_bytes() {
        assert_eq!(cache_size_preset_to_bytes("5 GB"), 5 * 1024 * 1024 * 1024);
        assert_eq!(cache_size_preset_to_bytes("10 GB"), 10 * 1024 * 1024 * 1024);
        assert_eq!(cache_size_preset_to_bytes("Unlimited"), 0);
        assert_eq!(cache_size_preset_to_bytes("nonsense"), 0);
    }

    #[test]
    fn presets_list_is_sorted_in_presentation_order() {
        // Sanity check — small set, deterministic order.
        assert_eq!(CACHE_SIZE_PRESETS.first(), Some(&"5 GB"));
        assert_eq!(CACHE_SIZE_PRESETS.last(), Some(&"Unlimited"));
    }
}
