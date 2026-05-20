//! Tests for likes, subscriptions, and blocked-video routes.
//!
//! The `like` handler accepts optional `{title, thumbnail_url}` in the
//! POST body. With no body it still succeeds (the row gets NULL
//! metadata columns). The `subscribe` and `block` handlers still do
//! best-effort discovery-sidecar lookups for metadata; when the sidecar
//! is unreachable (as in the unmocked harness) they degrade gracefully.

mod common;

use axum::http::StatusCode;
use common::boot_with_parent_and_child;
use hometube::models::account::AccountType;
use serde_json::json;

// ===========================================================================
// Likes
// ===========================================================================

#[tokio::test]
async fn like_creates_row_without_body() {
    let (app, _auth) = boot_with_parent_and_child(AccountType::Child).await;

    // No body supplied — the row gets NULL title/thumbnail columns and
    // the like still succeeds.
    let res = app.server.post("/api/likes/vid-liked").await;
    assert_eq!(res.status_code(), StatusCode::OK);
    let body: serde_json::Value = res.json();
    assert_eq!(body["video_id"], "vid-liked");
    assert!(body["video_title"].is_null());
    assert!(body["video_thumbnail_url"].is_null());
    assert_eq!(body["visible"], false); // not allowlisted
}

#[tokio::test]
async fn like_persists_client_supplied_metadata() {
    let (app, _auth) = boot_with_parent_and_child(AccountType::Child).await;

    let res = app
        .server
        .post("/api/likes/vid-meta")
        .json(&json!({
            "title": "Player-Supplied Title",
            "thumbnail_url": "https://thumb.test/x.jpg",
        }))
        .await;
    assert_eq!(res.status_code(), StatusCode::OK);
    let body: serde_json::Value = res.json();
    assert_eq!(body["video_title"], "Player-Supplied Title");
    assert_eq!(body["video_thumbnail_url"], "https://thumb.test/x.jpg");
}

#[tokio::test]
async fn relike_without_body_preserves_existing_metadata() {
    // Re-liking after a soft-unlike must not blank out previously-stored
    // title/thumbnail. The upsert's `COALESCE(excluded.x, video_likes.x)`
    // handles this so a second client that lacks the metadata (e.g. a
    // legacy build during a deploy window) can still re-like cleanly.
    let (app, _auth) = boot_with_parent_and_child(AccountType::Child).await;

    app.server
        .post("/api/likes/vid-keep")
        .json(&json!({
            "title": "Original Title",
            "thumbnail_url": "https://thumb.test/orig.jpg",
        }))
        .await;
    app.server.delete("/api/likes/vid-keep").await;

    // Re-like with no body.
    let res = app.server.post("/api/likes/vid-keep").await;
    assert_eq!(res.status_code(), StatusCode::OK);
    let body: serde_json::Value = res.json();
    assert_eq!(body["video_title"], "Original Title");
    assert_eq!(body["video_thumbnail_url"], "https://thumb.test/orig.jpg");
}

#[tokio::test]
async fn like_is_visible_when_allowlisted() {
    let (app, auth) = boot_with_parent_and_child(AccountType::Child).await;
    let child_id = auth.account_id;
    let parent_id = app.parent_id.unwrap();

    sqlx::query(
        "INSERT INTO allowlisted_videos (child_account_id, video_id, video_title, added_by) \
         VALUES (?, 'vid-vis', 'Title', ?)",
    )
    .bind(child_id)
    .bind(parent_id)
    .execute(&app.pool)
    .await
    .unwrap();

    let res = app.server.post("/api/likes/vid-vis").await;
    assert_eq!(res.status_code(), StatusCode::OK);
    let body: serde_json::Value = res.json();
    assert_eq!(body["visible"], true);
}

#[tokio::test]
async fn like_and_unlike_round_trip() {
    let (app, _auth) = boot_with_parent_and_child(AccountType::Child).await;

    // Like it.
    let res = app.server.post("/api/likes/vid-rt").await;
    assert_eq!(res.status_code(), StatusCode::OK);

    // List should show it.
    let res = app.server.get("/api/likes").await;
    let body: serde_json::Value = res.json();
    assert_eq!(body.as_array().unwrap().len(), 1);

    // Unlike it.
    let res = app.server.delete("/api/likes/vid-rt").await;
    assert_eq!(res.status_code(), StatusCode::NO_CONTENT);

    // List should be empty (soft-deleted).
    let res = app.server.get("/api/likes").await;
    let body: serde_json::Value = res.json();
    assert!(body.as_array().unwrap().is_empty());
}

#[tokio::test]
async fn like_idempotent_revive_after_unlike() {
    let (app, _auth) = boot_with_parent_and_child(AccountType::Child).await;

    app.server.post("/api/likes/vid-re").await;
    app.server.delete("/api/likes/vid-re").await;

    // Re-like should revive the row.
    let res = app.server.post("/api/likes/vid-re").await;
    assert_eq!(res.status_code(), StatusCode::OK);

    let res = app.server.get("/api/likes").await;
    let body: serde_json::Value = res.json();
    assert_eq!(body.as_array().unwrap().len(), 1);
}

// ===========================================================================
// Subscriptions
// ===========================================================================

#[tokio::test]
async fn subscriptions_list_empty_for_fresh_child() {
    let (app, _auth) = boot_with_parent_and_child(AccountType::Child).await;
    let res = app.server.get("/api/subscriptions").await;
    assert_eq!(res.status_code(), StatusCode::OK);
    let body: serde_json::Value = res.json();
    assert!(body.as_array().unwrap().is_empty());
}

#[tokio::test]
async fn subscribe_fails_gracefully_with_fake_api_key() {
    let (app, _auth) = boot_with_parent_and_child(AccountType::Child).await;
    // The YouTube lookup will fail because we have a fake API key.
    let res = app
        .server
        .post("/api/subscriptions")
        .json(&json!({ "channel_id": "UC_fake" }))
        .await;
    // Should return an error (YouTube returned non-success).
    let status = res.status_code().as_u16();
    assert!(status >= 400);
}

// ===========================================================================
// Blocked videos
// ===========================================================================

#[tokio::test]
async fn blocked_list_empty_for_fresh_child() {
    let (app, auth) = boot_with_parent_and_child(AccountType::Parent).await;
    let child_id = app.child_id.unwrap();
    let _ = auth;

    let res = app
        .server
        .get(&format!("/api/children/{child_id}/blocked"))
        .await;
    assert_eq!(res.status_code(), StatusCode::OK);
    let body: serde_json::Value = res.json();
    assert!(body.as_array().unwrap().is_empty());
}

#[tokio::test]
async fn block_and_unblock_round_trip() {
    let (app, _auth) = boot_with_parent_and_child(AccountType::Parent).await;
    let child_id = app.child_id.unwrap();

    // Block a video.
    let res = app
        .server
        .post(&format!("/api/children/{child_id}/blocked"))
        .json(&json!({ "video_id": "vid-block", "reason": "inappropriate" }))
        .await;
    assert_eq!(res.status_code(), StatusCode::OK);
    let body: serde_json::Value = res.json();
    assert_eq!(body["video_id"], "vid-block");
    assert_eq!(body["reason"], "inappropriate");

    // List should have it.
    let res = app
        .server
        .get(&format!("/api/children/{child_id}/blocked"))
        .await;
    let body: serde_json::Value = res.json();
    assert_eq!(body.as_array().unwrap().len(), 1);

    // Unblock.
    let res = app
        .server
        .delete(&format!("/api/children/{child_id}/blocked/vid-block"))
        .await;
    assert_eq!(res.status_code(), StatusCode::NO_CONTENT);

    // List should be empty now.
    let res = app
        .server
        .get(&format!("/api/children/{child_id}/blocked"))
        .await;
    let body: serde_json::Value = res.json();
    assert!(body.as_array().unwrap().is_empty());
}

#[tokio::test]
async fn block_for_non_child_is_400() {
    let (app, _auth) = boot_with_parent_and_child(AccountType::Parent).await;
    let parent_id = app.parent_id.unwrap();

    let res = app
        .server
        .post(&format!("/api/children/{parent_id}/blocked"))
        .json(&json!({ "video_id": "vid-x" }))
        .await;
    assert_eq!(res.status_code(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn block_without_reason() {
    let (app, _auth) = boot_with_parent_and_child(AccountType::Parent).await;
    let child_id = app.child_id.unwrap();

    let res = app
        .server
        .post(&format!("/api/children/{child_id}/blocked"))
        .json(&json!({ "video_id": "vid-noreason" }))
        .await;
    assert_eq!(res.status_code(), StatusCode::OK);
    let body: serde_json::Value = res.json();
    assert_eq!(body["video_id"], "vid-noreason");
    assert!(body["reason"].is_null());
}
