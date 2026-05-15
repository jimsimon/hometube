//! Extended video_cache service tests — covers get_or_extract cache layers,
//! TTL settings, cleanup logic, and size presets.

mod common;

use chrono::Utc;
use common::boot;
use hometube::services::video_cache::{
    cache_size_preset_to_bytes, clear_all, current_cache_size_label, evict_video_public,
    list_cached_videos, set_cache_size, set_ttl_hours, total_cache_bytes, total_segment_count,
    VideoCache, CACHE_SIZE_PRESETS, DEFAULT_CACHE_MAX_SIZE,
};

// ---------------------------------------------------------------------------
// VideoCache get_or_extract — DB layer
// ---------------------------------------------------------------------------

#[tokio::test]
async fn get_or_extract_uses_db_cache() {
    let app = boot().await;
    let cfg = hometube::config::Config::from_env().unwrap();

    // Seed a metadata cache row.
    let json = serde_json::json!({
        "id": "db-hit",
        "title": "From DB",
        "channel_id": "ch1",
        "formats": [],
        "thumbnails": [],
        "subtitles": {},
        "automatic_captions": {}
    });
    let expires_at = Utc::now().timestamp() + 3600;
    sqlx::query(
        "INSERT INTO video_metadata_cache (video_id, metadata_json, expires_at) VALUES (?, ?, ?)",
    )
    .bind("db-hit")
    .bind(json.to_string())
    .bind(expires_at)
    .execute(&app.pool)
    .await
    .unwrap();

    let cache = VideoCache::new();
    let result = cache
        .get_or_extract(&app.pool, &cfg, "db-hit")
        .await
        .unwrap();
    assert_eq!(result.id, "db-hit");
    assert_eq!(result.title.as_deref(), Some("From DB"));
}

#[tokio::test]
async fn get_or_extract_expired_db_row_is_miss() {
    let app = boot().await;
    let cfg = hometube::config::Config::from_env().unwrap();

    // Seed an expired row.
    let json = serde_json::json!({
        "id": "expired",
        "title": "Old",
        "formats": [],
        "thumbnails": [],
        "subtitles": {},
        "automatic_captions": {}
    });
    let expires_at = Utc::now().timestamp() - 100; // expired
    sqlx::query(
        "INSERT INTO video_metadata_cache (video_id, metadata_json, expires_at) VALUES (?, ?, ?)",
    )
    .bind("expired")
    .bind(json.to_string())
    .bind(expires_at)
    .execute(&app.pool)
    .await
    .unwrap();

    let cache = VideoCache::new();
    // This will try to call yt-dlp (which will fail), proving the DB
    // layer correctly returned None for the expired row.
    let result = cache.get_or_extract(&app.pool, &cfg, "expired").await;
    assert!(result.is_err());
}

// ---------------------------------------------------------------------------
// Cache settings
// ---------------------------------------------------------------------------

#[tokio::test]
async fn set_and_read_ttl_hours() {
    let app = boot().await;
    set_ttl_hours(&app.pool, 12).await.unwrap();
    let val: String =
        sqlx::query_scalar("SELECT value FROM app_config WHERE key = 'metadata_cache_ttl_hours'")
            .fetch_one(&app.pool)
            .await
            .unwrap();
    assert_eq!(val, "12");
}

#[tokio::test]
async fn set_cache_size_validates_input() {
    let app = boot().await;
    let err = set_cache_size(&app.pool, "999 TB").await;
    assert!(err.is_err());

    set_cache_size(&app.pool, "25 GB").await.unwrap();
    let label = current_cache_size_label(&app.pool).await;
    assert_eq!(label, "25 GB");
}

#[tokio::test]
async fn default_cache_size_is_50gb() {
    let app = boot().await;
    let label = current_cache_size_label(&app.pool).await;
    assert_eq!(label, DEFAULT_CACHE_MAX_SIZE);
}

// ---------------------------------------------------------------------------
// Cache size presets
// ---------------------------------------------------------------------------

#[test]
fn all_presets_have_valid_byte_values() {
    for &preset in CACHE_SIZE_PRESETS {
        let bytes = cache_size_preset_to_bytes(preset);
        if preset == "Unlimited" {
            assert_eq!(bytes, 0);
        } else {
            assert!(bytes > 0, "preset '{preset}' should have positive bytes");
        }
    }
}

// ---------------------------------------------------------------------------
// Segment cache operations
// ---------------------------------------------------------------------------

#[tokio::test]
async fn total_bytes_and_count_start_at_zero() {
    let app = boot().await;
    let bytes = total_cache_bytes(&app.pool).await.unwrap();
    let count = total_segment_count(&app.pool).await.unwrap();
    assert_eq!(bytes, 0);
    assert_eq!(count, 0);
}

#[tokio::test]
async fn list_cached_videos_empty_initially() {
    let app = boot().await;
    let videos = list_cached_videos(&app.pool).await.unwrap();
    assert!(videos.is_empty());
}

#[tokio::test]
async fn evict_video_no_op_when_not_in_cache() {
    let app = boot().await;
    let (segs, bytes) = evict_video_public(&app.pool, "nonexistent").await.unwrap();
    assert_eq!(segs, 0);
    assert_eq!(bytes, 0);
}

#[tokio::test]
async fn clear_all_removes_metadata_cache() {
    let app = boot().await;
    let json = serde_json::json!({"id": "to-clear", "formats": [], "thumbnails": [], "subtitles": {}, "automatic_captions": {}});
    let expires_at = Utc::now().timestamp() + 3600;
    sqlx::query(
        "INSERT INTO video_metadata_cache (video_id, metadata_json, expires_at) VALUES (?, ?, ?)",
    )
    .bind("to-clear")
    .bind(json.to_string())
    .bind(expires_at)
    .execute(&app.pool)
    .await
    .unwrap();

    clear_all(&app.pool).await.unwrap();

    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM video_metadata_cache")
        .fetch_one(&app.pool)
        .await
        .unwrap();
    assert_eq!(count, 0);
}

#[tokio::test]
async fn segment_cache_tracks_size() {
    let app = boot().await;

    // Manually insert a segment cache row (no actual file needed for the
    // DB-only size tracking tests).
    sqlx::query(
        "INSERT INTO segment_cache (video_id, format_id, segment_number, file_path, file_size_bytes) \
         VALUES ('vid-s', '137', 0, '/tmp/fake.seg', 1024)",
    )
    .execute(&app.pool)
    .await
    .unwrap();

    let bytes = total_cache_bytes(&app.pool).await.unwrap();
    assert_eq!(bytes, 1024);
    let count = total_segment_count(&app.pool).await.unwrap();
    assert_eq!(count, 1);

    let videos = list_cached_videos(&app.pool).await.unwrap();
    assert_eq!(videos.len(), 1);
    assert_eq!(videos[0].0, "vid-s");
    assert_eq!(videos[0].1, 1024);
    assert_eq!(videos[0].2, 1);
}

#[tokio::test]
async fn evict_video_removes_segments_and_metadata() {
    let app = boot().await;

    // Seed metadata + segment cache.
    let json = serde_json::json!({"id": "evict-me", "formats": [], "thumbnails": [], "subtitles": {}, "automatic_captions": {}});
    sqlx::query(
        "INSERT INTO video_metadata_cache (video_id, metadata_json, expires_at) VALUES (?, ?, ?)",
    )
    .bind("evict-me")
    .bind(json.to_string())
    .bind(Utc::now().timestamp() + 3600)
    .execute(&app.pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO segment_cache (video_id, format_id, segment_number, file_path, file_size_bytes) \
         VALUES ('evict-me', '137', 0, '/tmp/nonexistent.seg', 2048)",
    )
    .execute(&app.pool)
    .await
    .unwrap();

    let (segs, bytes) = evict_video_public(&app.pool, "evict-me").await.unwrap();
    assert_eq!(segs, 1);
    assert_eq!(bytes, 2048);

    let count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM video_metadata_cache WHERE video_id = 'evict-me'")
            .fetch_one(&app.pool)
            .await
            .unwrap();
    assert_eq!(count, 0);
}
