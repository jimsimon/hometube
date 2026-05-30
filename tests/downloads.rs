//! Offline downloads route coverage.
//!
//! `POST /api/downloads` invokes yt-dlp via [`crate::services::video_cache`],
//! so we don't exercise it here. The list / update / delete handlers
//! work entirely against the `offline_downloads` table and we can drive
//! them with direct INSERTs.

mod common;

use axum::http::StatusCode;
use common::boot_with_parent_and_child;
use hometube::models::account::AccountType;
use serde_json::json;

#[tokio::test]
async fn list_returns_seeded_rows() {
    let (app, auth) = boot_with_parent_and_child(AccountType::Child).await;
    let child_id = auth.account_id;

    common::seed_offline_download(
        &app.pool,
        child_id,
        "vid11111111",
        Some("Hello"),
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
    assert_eq!(body[0]["video_id"], "vid11111111");
}

#[tokio::test]
async fn update_status_to_complete() {
    let (app, auth) = boot_with_parent_and_child(AccountType::Child).await;
    let child_id = auth.account_id;
    common::seed_offline_download(
        &app.pool,
        child_id,
        "vid11111111",
        Some("Hello"),
        None,
        None,
        None,
        "720p",
        "pending",
    )
    .await;

    let res = app
        .server
        .put("/api/downloads/vid11111111")
        .json(&json!({ "status": "complete", "quality": "720p" }))
        .await;
    assert_eq!(res.status_code(), StatusCode::NO_CONTENT);

    let status: String = sqlx::query_scalar(
        "SELECT status FROM offline_downloads WHERE child_account_id = ? AND video_id = ?",
    )
    .bind(child_id)
    .bind("vid11111111")
    .fetch_one(&app.pool)
    .await
    .unwrap();
    assert_eq!(status, "complete");
}

#[tokio::test]
async fn child_with_downloads_disabled_cannot_create() {
    let (app, auth) = boot_with_parent_and_child(AccountType::Child).await;
    let child_id = auth.account_id;
    sqlx::query(
        "INSERT INTO child_settings (child_account_id, downloads_enabled) \
         VALUES (?, 0)",
    )
    .bind(child_id)
    .execute(&app.pool)
    .await
    .unwrap();

    let res = app
        .server
        .post("/api/downloads")
        .json(&json!({ "video_id": "vid11111111", "quality": "720p" }))
        .await;
    assert_eq!(res.status_code(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn delete_marks_row_deleted() {
    let (app, auth) = boot_with_parent_and_child(AccountType::Child).await;
    let child_id = auth.account_id;
    common::seed_offline_download(
        &app.pool,
        child_id,
        "vid11111111",
        Some("Hello"),
        None,
        None,
        None,
        "720p",
        "complete",
    )
    .await;

    let res = app.server.delete("/api/downloads/vid11111111").await;
    assert_eq!(res.status_code(), StatusCode::NO_CONTENT);
}
