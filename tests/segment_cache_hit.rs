//! Tests for the segment-cache hit path in `/api/proxy/format`.
//!
//! These tests seed cached chunks on disk + DB, then issue signed Range
//! requests and verify the handler serves bytes directly from cache
//! without touching any upstream service.
//!
//! Each test gets its own `TempDir` via `TestApp::cache_dir`, so there
//! is no shared state and no cleanup required.

mod common;

use std::time::Duration;

use axum::http::{header, HeaderValue, StatusCode};
use common::boot_with_parent_and_child;
use hometube::models::account::AccountType;
use hometube::services::dash::ensure_proxy_secret;
use hometube::services::segment_store::{chunk_path, store_chunk, CHUNK_SIZE};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// `lmt=` stamp baked into the seeded format URL, and the fingerprint
/// `segment_store::format_fingerprint` derives from it. Chunks seeded
/// with [`FINGERPRINT`] are "current"; anything else is stale.
const LMT: &str = "1758150000000000";
const FINGERPRINT: &str = "lmt:1758150000000000";

/// Helper to seed a video_metadata_cache row so `get_or_extract` succeeds
/// with a fake format. Without this, the handler returns 500 because yt-dlp
/// isn't available.
async fn seed_metadata(pool: &sqlx::SqlitePool, video_id: &str, format_id: &str) {
    seed_metadata_with_url(
        pool,
        video_id,
        format_id,
        &format!("https://example.com/fake?lmt={LMT}"),
    )
    .await;
}

async fn seed_metadata_with_url(
    pool: &sqlx::SqlitePool,
    video_id: &str,
    format_id: &str,
    url: &str,
) {
    let meta = serde_json::json!({
        "id": video_id,
        "title": "Test Video",
        "formats": [{
            "format_id": format_id,
            "ext": "webm",
            "url": url,
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
    common::mark_metadata_cache_current(pool).await;
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
    store_chunk(
        &app.pool,
        cache_dir,
        video_id,
        format_id,
        Some(FINGERPRINT),
        0,
        &chunk_data,
    )
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
    store_chunk(
        &app.pool,
        cache_dir,
        video_id,
        format_id,
        Some(FINGERPRINT),
        0,
        &chunk_data,
    )
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

// ---------------------------------------------------------------------------
// Stale-chunk handling: the on-disk copy belongs to an older encode of the
// upstream file (different `lmt=`), or predates fingerprinting entirely.
// ---------------------------------------------------------------------------

/// Wait (bounded) for background tasks — the tee writer and the stale
/// purge both run on spawned tasks after the response is returned.
async fn eventually<F, Fut>(mut check: F)
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    for _ in 0..100 {
        if check().await {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("condition not met within timeout");
}

async fn chunk_rows(pool: &sqlx::SqlitePool, video_id: &str) -> Vec<(i64, Option<String>)> {
    sqlx::query_as(
        "SELECT segment_number, fingerprint FROM segment_cache \
         WHERE video_id = ? ORDER BY segment_number",
    )
    .bind(video_id)
    .fetch_all(pool)
    .await
    .unwrap()
}

/// Stand up a wiremock "CDN" that answers one full-chunk Range request
/// with `upstream_bytes`, and seed metadata whose format URL points at it
/// with the current `lmt=`.
async fn mock_upstream(
    pool: &sqlx::SqlitePool,
    video_id: &str,
    format_id: &str,
    status: u16,
    upstream_bytes: Vec<u8>,
) -> MockServer {
    let server = MockServer::start().await;
    let total = CHUNK_SIZE * 2;
    let end = CHUNK_SIZE - 1;
    let mut tpl = ResponseTemplate::new(status)
        .insert_header("Content-Type", "video/webm")
        .set_body_bytes(upstream_bytes);
    if status == 206 {
        tpl = tpl
            .insert_header("Content-Range", format!("bytes 0-{end}/{total}").as_str())
            .insert_header("Accept-Ranges", "bytes");
    }
    Mock::given(method("GET"))
        .and(path("/videoplayback"))
        .respond_with(tpl)
        .mount(&server)
        .await;
    seed_metadata_with_url(
        pool,
        video_id,
        format_id,
        &format!("{}/videoplayback?clen={total}&lmt={LMT}", server.uri()),
    )
    .await;
    server
}

#[tokio::test]
async fn stale_chunk_from_old_encode_is_bypassed_and_replaced() {
    let (app, _auth) = boot_with_parent_and_child(AccountType::Child).await;

    let video_id = "stale-vid";
    let format_id = "313";
    let cache_dir = app.cache_dir.path().to_str().unwrap();

    // Upstream now serves the *new* encode: every byte is 0xBB.
    let fresh: Vec<u8> = vec![0xBB; CHUNK_SIZE as usize];
    let _upstream = mock_upstream(&app.pool, video_id, format_id, 206, fresh.clone()).await;

    // Disk still holds chunk 0 of the *old* encode (0xAA), plus a second
    // old chunk outside the requested range to prove the purge is
    // format-wide.
    let old: Vec<u8> = vec![0xAA; CHUNK_SIZE as usize];
    for cn in 0..2 {
        store_chunk(
            &app.pool,
            cache_dir,
            video_id,
            format_id,
            Some("lmt:1700000000000000"),
            cn,
            &old,
        )
        .await
        .unwrap();
    }

    let secret = ensure_proxy_secret(&app.pool).await.unwrap();
    let url = hometube::services::dash::build_format_proxy_url(&secret, video_id, format_id);
    let range_val = HeaderValue::from_str(&format!("bytes=0-{}", CHUNK_SIZE - 1)).unwrap();
    let res = app
        .server
        .get(&url)
        .add_header(header::RANGE, range_val)
        .await;

    // Served from upstream, not from the stale chunk.
    assert_eq!(res.status_code(), StatusCode::PARTIAL_CONTENT);
    assert_eq!(res.as_bytes().as_ref(), fresh.as_slice());

    // The old chunks are purged and chunk 0 is re-cached from the new
    // encode under the current fingerprint.
    eventually(|| async {
        chunk_rows(&app.pool, video_id).await == vec![(0, Some(FINGERPRINT.to_string()))]
    })
    .await;
    assert!(!chunk_path(cache_dir, video_id, format_id, 1).exists());
    let on_disk = tokio::fs::read(chunk_path(cache_dir, video_id, format_id, 0))
        .await
        .unwrap();
    assert_eq!(on_disk, fresh);

    // A second request for the same range is now a genuine cache hit.
    let range_val = HeaderValue::from_static("bytes=0-9");
    let res = app
        .server
        .get(&url)
        .add_header(header::RANGE, range_val)
        .await;
    assert_eq!(res.status_code(), StatusCode::PARTIAL_CONTENT);
    assert_eq!(res.as_bytes().as_ref(), &fresh[..10]);
    assert_eq!(
        res.header("content-range").to_str().unwrap(),
        format!("bytes 0-9/{}", CHUNK_SIZE * 2)
    );
}

#[tokio::test]
async fn legacy_chunk_without_fingerprint_is_not_served() {
    let (app, _auth) = boot_with_parent_and_child(AccountType::Child).await;

    let video_id = "legacy-vid";
    let format_id = "248";
    let cache_dir = app.cache_dir.path().to_str().unwrap();

    let fresh: Vec<u8> = vec![0xCC; CHUNK_SIZE as usize];
    let _upstream = mock_upstream(&app.pool, video_id, format_id, 206, fresh.clone()).await;

    // Chunk written before the fingerprint column existed (NULL).
    let old: Vec<u8> = vec![0xAA; CHUNK_SIZE as usize];
    store_chunk(&app.pool, cache_dir, video_id, format_id, None, 0, &old)
        .await
        .unwrap();

    let secret = ensure_proxy_secret(&app.pool).await.unwrap();
    let url = hometube::services::dash::build_format_proxy_url(&secret, video_id, format_id);
    let range_val = HeaderValue::from_str(&format!("bytes=0-{}", CHUNK_SIZE - 1)).unwrap();
    let res = app
        .server
        .get(&url)
        .add_header(header::RANGE, range_val)
        .await;

    assert_eq!(res.status_code(), StatusCode::PARTIAL_CONTENT);
    assert_eq!(res.as_bytes().as_ref(), fresh.as_slice());

    eventually(|| async {
        chunk_rows(&app.pool, video_id).await == vec![(0, Some(FINGERPRINT.to_string()))]
    })
    .await;
}

#[tokio::test]
async fn upstream_error_body_is_passed_through_and_not_cached() {
    let (app, _auth) = boot_with_parent_and_child(AccountType::Child).await;

    let video_id = "forbidden-vid";
    let format_id = "137";
    let cache_dir = app.cache_dir.path().to_str().unwrap();

    // A chunk-sized 403 body: before the guard, a full-chunk Range
    // request would have tee-cached this as chunk 0.
    let error_body: Vec<u8> = vec![b'x'; CHUNK_SIZE as usize];
    let _upstream = mock_upstream(&app.pool, video_id, format_id, 403, error_body).await;

    let secret = ensure_proxy_secret(&app.pool).await.unwrap();
    let url = hometube::services::dash::build_format_proxy_url(&secret, video_id, format_id);
    let range_val = HeaderValue::from_str(&format!("bytes=0-{}", CHUNK_SIZE - 1)).unwrap();
    let res = app
        .server
        .get(&url)
        .add_header(header::RANGE, range_val)
        .await;

    assert_eq!(res.status_code(), StatusCode::FORBIDDEN);
    assert_eq!(res.as_bytes().len(), CHUNK_SIZE as usize);

    // Give any (incorrect) tee writer a chance to run, then verify
    // nothing was cached.
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(chunk_rows(&app.pool, video_id).await.is_empty());
    assert!(!chunk_path(cache_dir, video_id, format_id, 0).exists());
}
