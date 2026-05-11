//! Coverage of the proxy endpoints' rate-limit + signature gates.
//!
//! These don't actually serve bytes (that requires yt-dlp), but they
//! still exercise:
//!
//! - the `rate_limit_proxies` middleware (always invoked, never blocks
//!   under a single request),
//! - the `dash::verify_query` constant-time check,
//! - the early returns for missing / bad query strings.

mod common;

use axum::http::StatusCode;
use common::boot_with_parent_and_child;
use hometube::models::account::AccountType;
use hometube::services::dash::{build_segment_proxy_url, ensure_proxy_secret};

#[tokio::test]
async fn segment_with_bad_signature_is_403() {
    let (app, _auth) = boot_with_parent_and_child(AccountType::Child).await;
    // Need a usage limit so we don't hit the cap. Skipping the limit
    // means the middleware lets us through for free.
    let res = app
        .server
        .get("/api/proxy/segment?video_id=abc&format=137&sq=42&sig=garbage")
        .await;
    assert_eq!(res.status_code(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn segment_with_valid_signature_passes_signing_gate() {
    let (app, _auth) = boot_with_parent_and_child(AccountType::Child).await;
    // Resolve the actual proxy secret used by the boot helper so we
    // can mint a signature the server will accept.
    let secret = ensure_proxy_secret(&app.pool).await.unwrap();
    let url = build_segment_proxy_url(&secret, "abc", "137", "42");
    // After a valid signature, the handler tries to read from cache /
    // yt-dlp. yt-dlp isn't available in the test environment, so we
    // expect a 5xx — but specifically *not* a 403, which would mean
    // the signature check failed.
    let res = app.server.get(&url).await;
    assert_ne!(
        res.status_code(),
        StatusCode::FORBIDDEN,
        "valid signature should pass the verification gate"
    );
}

#[tokio::test]
async fn audio_proxy_rejects_bad_signature() {
    let (app, _auth) = boot_with_parent_and_child(AccountType::Child).await;
    let res = app
        .server
        .get("/api/proxy/audio?video_id=abc&sig=bad")
        .await;
    let s = res.status_code();
    // 403 (bad sig) or 400 (missing required query). Either way, not
    // a 200.
    assert!(!s.is_success(), "got {s}");
}

#[tokio::test]
async fn segment_cache_hit_returns_file_bytes() {
    let (app, _auth) = boot_with_parent_and_child(AccountType::Child).await;
    let secret = ensure_proxy_secret(&app.pool).await.unwrap();

    // Write a small file to disk and register it in segment_cache.
    let tmp_dir = std::env::temp_dir();
    let path = tmp_dir.join("hometube-test-seg.bin");
    std::fs::write(&path, b"hello-segment").unwrap();
    let path_str = path.to_string_lossy().to_string();

    sqlx::query(
        "INSERT INTO segment_cache (video_id, format_id, segment_number, file_path, file_size_bytes) \
         VALUES (?, ?, ?, ?, ?)",
    )
    .bind("vid-cached")
    .bind("137")
    .bind(7_i64)
    .bind(&path_str)
    .bind(13_i64)
    .execute(&app.pool)
    .await
    .unwrap();

    let url = build_segment_proxy_url(&secret, "vid-cached", "137", "7");
    let res = app.server.get(&url).await;
    assert_eq!(res.status_code(), StatusCode::OK);
    let bytes = res.as_bytes();
    assert_eq!(bytes.as_ref(), b"hello-segment");

    let _ = std::fs::remove_file(path);
}

#[tokio::test]
async fn thumbnail_proxy_path_is_routed() {
    let (app, _auth) = boot_with_parent_and_child(AccountType::Child).await;
    // The thumbnail proxy needs `?key=...&sig=...` query params; we
    // pass garbage and confirm the route is reachable (gets a non-200
    // response from the handler rather than a 404 from the router).
    let res = app.server.get("/api/proxy/thumbnail/abc").await;
    let s = res.status_code();
    assert_ne!(s, StatusCode::NOT_FOUND);
}
