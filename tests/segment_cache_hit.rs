//! Tests for the segment-cache hit path in `/api/proxy/format`.
//!
//! These tests seed cached chunks on disk + DB, then issue signed Range
//! requests and verify the handler serves bytes directly from cache
//! without touching any upstream service.
//!
//! Each test gets its own `TempDir` via `TestApp::cache_dir`, so there
//! is no shared state and no cleanup required.

mod common;

use axum::http::{header, HeaderValue, StatusCode};
use common::boot_with_parent_and_child;
use hometube::models::account::AccountType;
use hometube::services::dash::ensure_proxy_secret;
use hometube::services::segment_store::{store_chunk, CHUNK_SIZE};

/// Helper to seed a video_metadata_cache row so `get_or_extract` succeeds
/// with a fake format. Without this, the handler returns 500 because yt-dlp
/// isn't available.
async fn seed_metadata(pool: &sqlx::SqlitePool, video_id: &str, format_id: &str) {
    let meta = serde_json::json!({
        "id": video_id,
        "title": "Test Video",
        "formats": [{
            "format_id": format_id,
            "ext": "webm",
            "url": "https://example.com/fake",
            "protocol": "https",
            "filesize": CHUNK_SIZE * 2,
            "vcodec": "vp9",
            "height": 720,
            "width": 1280,
            "tbr": 2500.0
        }],
        "thumbnails": []
    });
    let now = chrono::Utc::now().timestamp();
    let expires_at = now + 3600;
    sqlx::query(
        "INSERT OR REPLACE INTO video_metadata_cache (video_id, metadata_json, cached_at, expires_at) \
         VALUES (?, ?, ?, ?)",
    )
    .bind(video_id)
    .bind(meta.to_string())
    .bind(now)
    .bind(expires_at)
    .execute(pool)
    .await
    .unwrap();
}

/// Seed format_box_ranges with total_bytes so cache-hit responses can
/// produce a proper Content-Range header.
async fn seed_total_bytes(pool: &sqlx::SqlitePool, video_id: &str, format_id: &str, total: i64) {
    sqlx::query(
        "INSERT OR REPLACE INTO format_box_ranges \
         (video_id, format_id, init_start, init_end, index_start, index_end, total_bytes) \
         VALUES (?, ?, 0, 511, 512, 4095, ?)",
    )
    .bind(video_id)
    .bind(format_id)
    .bind(total)
    .execute(pool)
    .await
    .unwrap();
}

#[tokio::test]
async fn cache_hit_serves_bytes_from_disk() {
    let (app, _auth) = boot_with_parent_and_child(AccountType::Child).await;

    let video_id = "test-vid-1";
    let format_id = "137";
    let cache_dir = app.cache_dir.path().to_str().unwrap();

    // Seed metadata so the handler can look up the format.
    seed_metadata(&app.pool, video_id, format_id).await;
    seed_total_bytes(&app.pool, video_id, format_id, CHUNK_SIZE as i64 * 2).await;

    // Store a complete chunk on disk + DB.
    let chunk_data: Vec<u8> = (0..CHUNK_SIZE).map(|i| (i % 256) as u8).collect();
    store_chunk(&app.pool, cache_dir, video_id, format_id, 0, &chunk_data)
        .await
        .unwrap();

    // Build a valid signed proxy URL.
    let secret = ensure_proxy_secret(&app.pool).await.unwrap();
    let url = hometube::services::dash::build_format_proxy_url(&secret, video_id, format_id);

    // Make a Range request covering chunk 0.
    let end_byte = CHUNK_SIZE - 1;
    let range_val = HeaderValue::from_str(&format!("bytes=0-{end_byte}")).unwrap();
    let res = app
        .server
        .get(&url)
        .add_header(header::RANGE, range_val)
        .await;

    assert_eq!(res.status_code(), StatusCode::PARTIAL_CONTENT);

    // Verify Content-Range header.
    let content_range = res.header("content-range");
    let expected_range = format!("bytes 0-{}/{}", end_byte, CHUNK_SIZE * 2);
    assert_eq!(content_range.to_str().unwrap(), expected_range);

    // Verify body is the cached chunk data.
    let body = res.as_bytes();
    assert_eq!(body.len(), CHUNK_SIZE as usize);
    assert_eq!(body[0], 0);
    assert_eq!(body[100], 100);
    assert_eq!(body[255], 255);
}

#[tokio::test]
async fn cache_miss_falls_through_to_upstream() {
    let (app, _auth) = boot_with_parent_and_child(AccountType::Child).await;

    let video_id = "miss-vid";
    let format_id = "248";

    // Seed metadata so the handler can find the format.
    seed_metadata(&app.pool, video_id, format_id).await;

    // Build a valid signed proxy URL.
    let secret = ensure_proxy_secret(&app.pool).await.unwrap();
    let url = hometube::services::dash::build_format_proxy_url(&secret, video_id, format_id);

    // Make a Range request — no chunks cached, so it falls through to
    // upstream. The upstream URL is fake (example.com), so it will fail.
    let range_val = HeaderValue::from_static("bytes=0-1023");
    let res = app
        .server
        .get(&url)
        .add_header(header::RANGE, range_val)
        .await;

    // Should NOT be 403 (that would mean sig check failed) or 206 (cache hit).
    // The exact code depends on whether the handler resolves the format and
    // attempts the upstream fetch.
    assert_ne!(res.status_code(), StatusCode::FORBIDDEN);
    assert_ne!(res.status_code(), StatusCode::PARTIAL_CONTENT);
}

#[tokio::test]
async fn cache_hit_partial_range_within_chunk() {
    let (app, _auth) = boot_with_parent_and_child(AccountType::Child).await;

    let video_id = "partial-vid";
    let format_id = "251";
    let cache_dir = app.cache_dir.path().to_str().unwrap();

    seed_metadata(&app.pool, video_id, format_id).await;
    seed_total_bytes(&app.pool, video_id, format_id, CHUNK_SIZE as i64).await;

    // Store chunk 0 with known data.
    let chunk_data: Vec<u8> = vec![42u8; CHUNK_SIZE as usize];
    store_chunk(&app.pool, cache_dir, video_id, format_id, 0, &chunk_data)
        .await
        .unwrap();

    let secret = ensure_proxy_secret(&app.pool).await.unwrap();
    let url = hometube::services::dash::build_format_proxy_url(&secret, video_id, format_id);

    // Request only bytes 100-199 (a small slice within chunk 0).
    let range_val = HeaderValue::from_static("bytes=100-199");
    let res = app
        .server
        .get(&url)
        .add_header(header::RANGE, range_val)
        .await;

    assert_eq!(res.status_code(), StatusCode::PARTIAL_CONTENT);

    let body = res.as_bytes();
    assert_eq!(body.len(), 100);
    assert!(body.iter().all(|&b| b == 42));
}
