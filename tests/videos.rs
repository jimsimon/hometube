//! `routes/videos.rs` coverage by pre-populating the metadata cache.
//!
//! The handlers normally call yt-dlp via the [`VideoCache`] layer. We
//! sidestep that by inserting a row into `video_metadata_cache` whose
//! JSON matches the [`ExtractResult`] schema. The DB-cache path then
//! returns the seeded row instead of shelling out.

mod common;

use axum::http::StatusCode;
use chrono::Utc;
use common::boot_with_parent_and_child;
use hometube::models::account::AccountType;

/// Seed `video_metadata_cache` with a tiny `ExtractResult`-shaped JSON
/// blob for the given video. Returns nothing — the test asserts via
/// the API.
async fn seed_metadata(pool: &sqlx::SqlitePool, video_id: &str, channel_id: &str) {
    let json = serde_json::json!({
        "id": video_id,
        "title": "Test Video",
        "channel_id": channel_id,
        "channel_title": "Test Channel",
        "duration": 123.5,
        "thumbnails": [
            {"url": "http://thumb.example/maxres.jpg", "width": 1280, "height": 720}
        ],
        "thumbnail": "http://thumb.example/default.jpg",
        "formats": [],
        "subtitles": {},
        "automatic_captions": {}
    });
    let expires_at = Utc::now().timestamp() + 3600;
    sqlx::query(
        "INSERT INTO video_metadata_cache (video_id, metadata_json, expires_at) \
         VALUES (?, ?, ?)",
    )
    .bind(video_id)
    .bind(json.to_string())
    .bind(expires_at)
    .execute(pool)
    .await
    .unwrap();
}

#[tokio::test]
async fn metadata_for_allowlisted_video() {
    let (app, auth) = boot_with_parent_and_child(AccountType::Child).await;
    let child_id = auth.account_id;
    let parent_id = app.parent_id.unwrap();

    seed_metadata(&app.pool, "vid-1", "chan-1").await;
    sqlx::query(
        "INSERT INTO allowlisted_videos (child_account_id, video_id, video_title, added_by) \
         VALUES (?, 'vid-1', 'Title', ?)",
    )
    .bind(child_id)
    .bind(parent_id)
    .execute(&app.pool)
    .await
    .unwrap();

    let res = app.server.get("/api/videos/vid-1").await;
    assert_eq!(res.status_code(), StatusCode::OK);
    let body: serde_json::Value = res.json();
    assert_eq!(body["id"], "vid-1");
    assert_eq!(body["title"], "Test Video");
    assert_eq!(body["channel_id"], "chan-1");
}

#[tokio::test]
async fn metadata_denies_blocked_video() {
    let (app, auth) = boot_with_parent_and_child(AccountType::Child).await;
    let child_id = auth.account_id;
    let parent_id = app.parent_id.unwrap();

    seed_metadata(&app.pool, "vid-bad", "chan-1").await;
    // Allowlist + then block. Block wins.
    sqlx::query(
        "INSERT INTO allowlisted_videos (child_account_id, video_id, video_title, added_by) \
         VALUES (?, 'vid-bad', 'Title', ?)",
    )
    .bind(child_id)
    .bind(parent_id)
    .execute(&app.pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO blocked_videos (child_account_id, video_id, blocked_by) \
         VALUES (?, 'vid-bad', ?)",
    )
    .bind(child_id)
    .bind(parent_id)
    .execute(&app.pool)
    .await
    .unwrap();

    let res = app.server.get("/api/videos/vid-bad").await;
    assert_eq!(res.status_code(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn parent_can_read_metadata_without_allowlist() {
    let (app, _auth) = boot_with_parent_and_child(AccountType::Parent).await;
    seed_metadata(&app.pool, "vid-1", "chan-1").await;
    let res = app.server.get("/api/videos/vid-1").await;
    assert_eq!(res.status_code(), StatusCode::OK);
}

#[tokio::test]
async fn list_captions_returns_empty_when_none() {
    let (app, auth) = boot_with_parent_and_child(AccountType::Child).await;
    let child_id = auth.account_id;
    let parent_id = app.parent_id.unwrap();

    seed_metadata(&app.pool, "vid-1", "chan-1").await;
    sqlx::query(
        "INSERT INTO allowlisted_videos (child_account_id, video_id, video_title, added_by) \
         VALUES (?, 'vid-1', 'Title', ?)",
    )
    .bind(child_id)
    .bind(parent_id)
    .execute(&app.pool)
    .await
    .unwrap();

    let res = app.server.get("/api/videos/vid-1/captions").await;
    assert_eq!(res.status_code(), StatusCode::OK);
    let body: serde_json::Value = res.json();
    // The seeded ExtractResult has empty subtitles + automatic_captions.
    let arr = body.as_array().unwrap();
    assert!(arr.is_empty());
}

#[tokio::test]
async fn stream_manifest_404_when_no_usable_formats() {
    let (app, auth) = boot_with_parent_and_child(AccountType::Child).await;
    let child_id = auth.account_id;
    let parent_id = app.parent_id.unwrap();

    seed_metadata(&app.pool, "vid-1", "chan-1").await;
    sqlx::query(
        "INSERT INTO allowlisted_videos (child_account_id, video_id, video_title, added_by) \
         VALUES (?, 'vid-1', 'Title', ?)",
    )
    .bind(child_id)
    .bind(parent_id)
    .execute(&app.pool)
    .await
    .unwrap();

    let res = app
        .server
        .get("/api/videos/vid-1/stream/manifest.mpd")
        .await;
    // No usable formats in the seeded metadata → 404 from the handler.
    assert_eq!(res.status_code(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn parent_preview_video_uses_seeded_cache() {
    let (app, _auth) = boot_with_parent_and_child(AccountType::Parent).await;
    seed_metadata(&app.pool, "vid-1", "chan-1").await;
    let res = app.server.get("/api/preview/video/vid-1").await;
    assert_eq!(res.status_code(), StatusCode::OK);
    let body: serde_json::Value = res.json();
    assert_eq!(body["id"], "vid-1");
}

#[tokio::test]
async fn stream_for_allowlisted_video_returns_formats() {
    let (app, auth) = boot_with_parent_and_child(AccountType::Child).await;
    let child_id = auth.account_id;
    let parent_id = app.parent_id.unwrap();

    seed_metadata(&app.pool, "vid-1", "chan-1").await;
    sqlx::query(
        "INSERT INTO allowlisted_videos (child_account_id, video_id, video_title, added_by) \
         VALUES (?, 'vid-1', 'Title', ?)",
    )
    .bind(child_id)
    .bind(parent_id)
    .execute(&app.pool)
    .await
    .unwrap();

    let res = app.server.get("/api/videos/vid-1/stream").await;
    assert_eq!(res.status_code(), StatusCode::OK);
    let body: serde_json::Value = res.json();
    assert!(body["formats"].is_array());
}

#[tokio::test]
async fn stream_applies_max_quality_cap_for_child() {
    let (app, auth) = boot_with_parent_and_child(AccountType::Child).await;
    let child_id = auth.account_id;
    let parent_id = app.parent_id.unwrap();

    // Seed metadata with a mix of heights so the cap filters some out.
    let json = serde_json::json!({
        "id": "vid-2",
        "title": "T",
        "channel_id": "ch",
        "channel_title": "C",
        "duration": 60.0,
        "thumbnails": [],
        "formats": [
            {"format_id": "137", "height": 1080, "url": "https://x/y"},
            {"format_id": "136", "height": 720,  "url": "https://x/y"},
            {"format_id": "135", "height": 480,  "url": "https://x/y"},
            {"format_id": "140", "url": "https://x/audio"}
        ],
        "subtitles": {},
        "automatic_captions": {}
    });
    let expires_at = Utc::now().timestamp() + 3600;
    sqlx::query(
        "INSERT INTO video_metadata_cache (video_id, metadata_json, expires_at) \
         VALUES (?, ?, ?)",
    )
    .bind("vid-2")
    .bind(json.to_string())
    .bind(expires_at)
    .execute(&app.pool)
    .await
    .unwrap();

    sqlx::query(
        "INSERT INTO allowlisted_videos (child_account_id, video_id, video_title, added_by) \
         VALUES (?, 'vid-2', 'Title', ?)",
    )
    .bind(child_id)
    .bind(parent_id)
    .execute(&app.pool)
    .await
    .unwrap();

    // Set a 720p cap on the child.
    sqlx::query(
        "INSERT INTO child_settings (child_account_id, max_quality) \
         VALUES (?, '720p') ON CONFLICT(child_account_id) DO UPDATE SET max_quality = '720p'",
    )
    .bind(child_id)
    .execute(&app.pool)
    .await
    .unwrap();

    let res = app.server.get("/api/videos/vid-2/stream").await;
    assert_eq!(res.status_code(), StatusCode::OK);
    let body: serde_json::Value = res.json();
    let heights: Vec<Option<i64>> = body["formats"]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| f["height"].as_i64())
        .collect();
    // No height > 720 makes it through.
    for h in heights.iter().flatten() {
        assert!(*h <= 720, "got height {h} despite 720p cap");
    }
    // The 1080p row was filtered out.
    assert!(!heights.contains(&Some(1080)));
    // Audio-only (no height) survives.
    assert!(heights.contains(&None));
}
