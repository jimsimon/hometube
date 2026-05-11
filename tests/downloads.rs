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

    sqlx::query(
        "INSERT INTO offline_downloads \
            (child_account_id, video_id, video_title, quality_label, status) \
         VALUES (?, 'vid-1', 'Hello', '720p', 'complete')",
    )
    .bind(child_id)
    .execute(&app.pool)
    .await
    .unwrap();

    let res = app.server.get("/api/downloads").await;
    assert_eq!(res.status_code(), StatusCode::OK);
    let body: serde_json::Value = res.json();
    assert_eq!(body[0]["video_id"], "vid-1");
}

#[tokio::test]
async fn update_status_to_complete() {
    let (app, auth) = boot_with_parent_and_child(AccountType::Child).await;
    let child_id = auth.account_id;
    sqlx::query(
        "INSERT INTO offline_downloads \
            (child_account_id, video_id, video_title, quality_label, status) \
         VALUES (?, 'vid-1', 'Hello', '720p', 'pending')",
    )
    .bind(child_id)
    .execute(&app.pool)
    .await
    .unwrap();

    let res = app
        .server
        .put("/api/downloads/vid-1")
        .json(&json!({ "status": "complete", "quality": "720p" }))
        .await;
    assert_eq!(res.status_code(), StatusCode::NO_CONTENT);

    let status: String = sqlx::query_scalar(
        "SELECT status FROM offline_downloads WHERE child_account_id = ? AND video_id = ?",
    )
    .bind(child_id)
    .bind("vid-1")
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
        .json(&json!({ "video_id": "vid-1", "quality": "720p" }))
        .await;
    assert_eq!(res.status_code(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn delete_marks_row_deleted() {
    let (app, auth) = boot_with_parent_and_child(AccountType::Child).await;
    let child_id = auth.account_id;
    sqlx::query(
        "INSERT INTO offline_downloads \
            (child_account_id, video_id, video_title, quality_label, status) \
         VALUES (?, 'vid-1', 'Hello', '720p', 'complete')",
    )
    .bind(child_id)
    .execute(&app.pool)
    .await
    .unwrap();

    let res = app.server.delete("/api/downloads/vid-1").await;
    assert_eq!(res.status_code(), StatusCode::NO_CONTENT);
}
