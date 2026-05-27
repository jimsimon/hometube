//! Extended offline-downloads tests covering create, update (without quality),
//! stream, and parent access.

mod common;

use axum::http::StatusCode;
use chrono::Utc;
use common::boot_with_parent_and_child;
use hometube::models::account::AccountType;
use serde_json::json;

/// Seed `video_metadata_cache` with a progressive format suitable for downloads.
async fn seed_downloadable_video(pool: &sqlx::SqlitePool, video_id: &str) {
    let json = serde_json::json!({
        "id": video_id,
        "title": "Downloadable Video",
        "channel_id": "chan-dl",
        "channel_title": "Download Channel",
        "duration": 300.0,
        "thumbnails": [
            {"url": "http://thumb.example/dl.jpg", "width": 1280, "height": 720}
        ],
        "thumbnail": "http://thumb.example/dl.jpg",
        "formats": [
            {"format_id": "18", "height": 360, "width": 640, "url": "https://dl.example/360p.mp4", "vcodec": "avc1", "acodec": "aac"},
            {"format_id": "22", "height": 720, "width": 1280, "url": "https://dl.example/720p.mp4", "vcodec": "avc1", "acodec": "aac"}
        ],
        "subtitles": {},
        "automatic_captions": {}
    });
    let expires_at = Utc::now().timestamp() + 3600;
    sqlx::query(
        "INSERT INTO video_metadata_cache (video_id, metadata_json, expires_at) \
         VALUES (?, ?, ?) ON CONFLICT(video_id) DO UPDATE SET metadata_json = excluded.metadata_json, expires_at = excluded.expires_at",
    )
    .bind(video_id)
    .bind(json.to_string())
    .bind(expires_at)
    .execute(pool)
    .await
    .unwrap();
}

// ---------------------------------------------------------------------------
// POST /api/downloads (create)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn create_download_for_allowlisted_video() {
    let (app, auth) = boot_with_parent_and_child(AccountType::Child).await;
    let child_id = auth.account_id;
    let parent_id = app.parent_id.unwrap();

    seed_downloadable_video(&app.pool, "dlvid111111").await;
    common::allowlist_video(
        &app.pool,
        child_id,
        parent_id,
        "dlvid111111",
        Some("DL Title"),
        None,
    )
    .await;
    // Downloads are fail-closed; flip the flag on for this child.
    sqlx::query(
        "INSERT INTO child_settings (child_account_id, downloads_enabled) VALUES (?, 1) \
         ON CONFLICT(child_account_id) DO UPDATE SET downloads_enabled = 1",
    )
    .bind(child_id)
    .execute(&app.pool)
    .await
    .unwrap();

    let res = app
        .server
        .post("/api/downloads")
        .json(&json!({ "video_id": "dlvid111111", "quality": "720p" }))
        .await;
    assert_eq!(res.status_code(), StatusCode::OK);
    let body: serde_json::Value = res.json();
    assert_eq!(body["video_id"], "dlvid111111");
    assert_eq!(body["quality"], "720p");
    assert!(body["stream_url"].as_str().unwrap().contains("dlvid111111"));
}

#[tokio::test]
async fn create_download_denied_for_non_allowlisted_video() {
    let (app, _auth) = boot_with_parent_and_child(AccountType::Child).await;
    seed_downloadable_video(&app.pool, "dlviddenied").await;

    let res = app
        .server
        .post("/api/downloads")
        .json(&json!({ "video_id": "dlviddenied", "quality": "720p" }))
        .await;
    assert_eq!(res.status_code(), StatusCode::FORBIDDEN);
}

// ---------------------------------------------------------------------------
// PUT /api/downloads/:videoId (update without quality)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn update_without_quality_updates_all_rows() {
    let (app, auth) = boot_with_parent_and_child(AccountType::Child).await;
    let child_id = auth.account_id;

    common::seed_offline_download(
        &app.pool,
        child_id,
        "vidup111111",
        Some("Hello"),
        None,
        None,
        None,
        "720p",
        "pending",
    )
    .await;
    common::seed_offline_download(
        &app.pool,
        child_id,
        "vidup111111",
        Some("Hello"),
        None,
        None,
        None,
        "480p",
        "pending",
    )
    .await;

    // Update without specifying quality → both rows are updated.
    let res = app
        .server
        .put("/api/downloads/vidup111111")
        .json(&json!({ "status": "complete" }))
        .await;
    assert_eq!(res.status_code(), StatusCode::NO_CONTENT);

    let count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM offline_downloads WHERE child_account_id = ? AND video_id = ? AND status = 'complete'",
    )
    .bind(child_id)
    .bind("vidup111111")
    .fetch_one(&app.pool)
    .await
    .unwrap();
    assert_eq!(count, 2);
}

// ---------------------------------------------------------------------------
// Downloads list filtering
// ---------------------------------------------------------------------------

#[tokio::test]
async fn list_excludes_deleted_downloads() {
    let (app, auth) = boot_with_parent_and_child(AccountType::Child).await;
    let child_id = auth.account_id;

    common::seed_offline_download(
        &app.pool,
        child_id,
        "viddel11111",
        Some("Deleted"),
        None,
        None,
        None,
        "720p",
        "deleted",
    )
    .await;
    common::seed_offline_download(
        &app.pool,
        child_id,
        "vidkept1111",
        Some("Kept"),
        None,
        None,
        None,
        "720p",
        "complete",
    )
    .await;

    let res = app.server.get("/api/downloads").await;
    assert_eq!(res.status_code(), StatusCode::OK);
    let body: serde_json::Value = res.json();
    let arr = body.as_array().unwrap();
    assert_eq!(arr.len(), 1);
    assert_eq!(arr[0]["video_id"], "vidkept1111");
}

// ---------------------------------------------------------------------------
// Downloads list is initially empty
// ---------------------------------------------------------------------------

#[tokio::test]
async fn downloads_list_initially_empty() {
    let (app, _auth) = boot_with_parent_and_child(AccountType::Child).await;
    let res = app.server.get("/api/downloads").await;
    assert_eq!(res.status_code(), StatusCode::OK);
    let body: serde_json::Value = res.json();
    assert!(body.as_array().unwrap().is_empty());
}

// ---------------------------------------------------------------------------
// Stream endpoint (downloads)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn stream_download_disabled_returns_403() {
    let (app, auth) = boot_with_parent_and_child(AccountType::Child).await;
    let child_id = auth.account_id;

    sqlx::query("INSERT INTO child_settings (child_account_id, downloads_enabled) VALUES (?, 0)")
        .bind(child_id)
        .execute(&app.pool)
        .await
        .unwrap();

    seed_downloadable_video(&app.pool, "dl-stream-1").await;

    let res = app
        .server
        .get("/api/downloads/dl-stream-1/stream?quality=720p")
        .await;
    assert_eq!(res.status_code(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn stream_download_denied_for_non_allowlisted() {
    let (app, _auth) = boot_with_parent_and_child(AccountType::Child).await;
    seed_downloadable_video(&app.pool, "dl-stream-2").await;

    let res = app
        .server
        .get("/api/downloads/dl-stream-2/stream?quality=720p")
        .await;
    assert_eq!(res.status_code(), StatusCode::FORBIDDEN);
}
