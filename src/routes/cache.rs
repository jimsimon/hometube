//! Segment-cache management API (parent only).
//!
//! Wraps [`crate::services::video_cache`] helpers as JSON endpoints.

use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    Json,
};
use serde::{Deserialize, Serialize};
use std::sync::atomic::Ordering;

use crate::error::{AppError, AppResult};
use crate::services::cron::{CACHE_HIT_COUNTER, CACHE_MISS_COUNTER};
use crate::services::video_cache;
use crate::state::AppState;

#[derive(Debug, Serialize)]
pub struct CacheStats {
    pub total_bytes: u64,
    pub segment_count: i64,
    pub video_count: i64,
    pub hit_count: u64,
    pub miss_count: u64,
    pub hit_rate: f64,
    pub max_size_label: String,
    pub max_size_bytes: u64,
    /// `true` when the configured cache size is "Unlimited" (no LRU
    /// eviction will run). Convenience for the UI so it can avoid
    /// rendering a progress bar against a zero limit.
    pub unlimited: bool,
    pub top_videos: Vec<CachedVideoSummary>,
}

#[derive(Debug, Serialize)]
pub struct CachedVideoSummary {
    pub video_id: String,
    pub total_bytes: i64,
    pub segment_count: i64,
}

/// `GET /api/cache/stats`.
pub async fn stats(State(state): State<AppState>) -> AppResult<Json<CacheStats>> {
    let total_bytes = video_cache::total_cache_bytes(&state.db).await?;
    let segment_count = video_cache::total_segment_count(&state.db).await?;
    let videos = video_cache::list_cached_videos(&state.db).await?;
    let video_count = videos.len() as i64;
    let top_videos = videos
        .iter()
        .take(10)
        .map(|(id, bytes, segs)| CachedVideoSummary {
            video_id: id.clone(),
            total_bytes: *bytes,
            segment_count: *segs,
        })
        .collect();
    let hits = CACHE_HIT_COUNTER.load(Ordering::Relaxed);
    let misses = CACHE_MISS_COUNTER.load(Ordering::Relaxed);
    let hit_rate = if hits + misses == 0 {
        0.0
    } else {
        hits as f64 / (hits + misses) as f64
    };
    let max_size_label = video_cache::current_cache_size_label(&state.db).await;
    let max_size_bytes = video_cache::cache_size_preset_to_bytes(&max_size_label);
    let unlimited = max_size_label.eq_ignore_ascii_case("Unlimited");
    Ok(Json(CacheStats {
        total_bytes,
        segment_count,
        video_count,
        hit_count: hits,
        miss_count: misses,
        hit_rate,
        max_size_label,
        max_size_bytes,
        unlimited,
        top_videos,
    }))
}

#[derive(Debug, Serialize)]
pub struct EvictionEntry {
    pub id: i64,
    pub video_id: String,
    pub segment_count: i64,
    pub bytes_freed: i64,
    pub reason: String,
    pub evicted_at: i64,
}

#[derive(Debug, Deserialize, Default)]
pub struct EvictionsQuery {
    #[serde(default)]
    pub limit: Option<i64>,
}

/// `GET /api/cache/evictions?limit=50` — recent cache evictions, newest
/// first. Each row includes the reason it was evicted (manual, clear-all,
/// not_allowlisted, or lru_size_limit).
pub async fn recent_evictions(
    State(state): State<AppState>,
    Query(params): Query<EvictionsQuery>,
) -> AppResult<Json<Vec<EvictionEntry>>> {
    let limit = params.limit.unwrap_or(50).clamp(1, 500);
    let rows = video_cache::recent_evictions(&state.db, limit).await?;
    Ok(Json(
        rows.into_iter()
            .map(|r| EvictionEntry {
                id: r.id,
                video_id: r.video_id,
                segment_count: r.segment_count,
                bytes_freed: r.bytes_freed,
                reason: r.reason,
                evicted_at: r.evicted_at,
            })
            .collect(),
    ))
}

#[derive(Debug, Serialize)]
pub struct CacheSettings {
    pub max_size: String,
    pub metadata_ttl_hours: i64,
}

#[derive(Debug, Deserialize)]
pub struct UpdateCacheSettings {
    #[serde(default)]
    pub max_size: Option<String>,
    #[serde(default)]
    pub metadata_ttl_hours: Option<i64>,
}

/// `GET /api/cache/settings`.
pub async fn get_settings(State(state): State<AppState>) -> AppResult<Json<CacheSettings>> {
    let label = video_cache::current_cache_size_label(&state.db).await;
    let ttl: i64 = crate::services::setup::get_config_value(
        &state.db,
        video_cache::KEY_METADATA_CACHE_TTL_HOURS,
    )
    .await?
    .and_then(|s| s.parse().ok())
    .unwrap_or(video_cache::DEFAULT_TTL_HOURS);
    Ok(Json(CacheSettings {
        max_size: label,
        metadata_ttl_hours: ttl,
    }))
}

/// `PUT /api/cache/settings`.
pub async fn update_settings(
    State(state): State<AppState>,
    Json(body): Json<UpdateCacheSettings>,
) -> AppResult<Json<CacheSettings>> {
    if let Some(label) = body.max_size {
        if !video_cache::CACHE_SIZE_PRESETS.iter().any(|p| *p == label) {
            return Err(AppError::BadRequest(format!(
                "max_size must be one of: {}",
                video_cache::CACHE_SIZE_PRESETS.join(", ")
            )));
        }
        video_cache::set_cache_size(&state.db, &label).await?;
    }
    if let Some(ttl) = body.metadata_ttl_hours {
        if !(1..=168).contains(&ttl) {
            return Err(AppError::BadRequest(
                "metadata_ttl_hours must be between 1 and 168".into(),
            ));
        }
        video_cache::set_ttl_hours(&state.db, ttl).await?;
    }
    get_settings(State(state)).await
}

/// `GET /api/cache/videos`.
pub async fn list_videos(
    State(state): State<AppState>,
) -> AppResult<Json<Vec<CachedVideoSummary>>> {
    let videos = video_cache::list_cached_videos(&state.db).await?;
    Ok(Json(
        videos
            .into_iter()
            .map(|(id, bytes, segs)| CachedVideoSummary {
                video_id: id,
                total_bytes: bytes,
                segment_count: segs,
            })
            .collect(),
    ))
}

/// `DELETE /api/cache/videos/:videoId`.
pub async fn delete_video(
    State(state): State<AppState>,
    Path(video_id): Path<String>,
) -> AppResult<StatusCode> {
    video_cache::evict_video_public(&state.db, &video_id).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// `POST /api/cache/clear`.
pub async fn clear_all(State(state): State<AppState>) -> AppResult<StatusCode> {
    video_cache::clear_all(&state.db).await?;
    Ok(StatusCode::NO_CONTENT)
}
