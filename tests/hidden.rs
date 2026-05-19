//! Tests for per-child "Hidden Videos".
//!
//! Hidden is child-initiated and per-child. We assert:
//! - auth required,
//! - basic CRUD round-trip,
//! - hiding by child A does not affect child B,
//! - `can_child_view` denies hidden videos,
//! - unhiding restores visibility.

mod common;

use axum::http::StatusCode;
use common::{
    boot_setup_complete, boot_with_parent_and_child, insert_account, mint_session_cookie,
};
use hometube::models::account::AccountType;
use hometube::services::access::{can_child_view, is_hidden_for_child};
use serde_json::json;
use tower_cookies::cookie::Cookie;

#[tokio::test]
async fn hide_requires_auth() {
    let (mut app, _) = boot_with_parent_and_child(AccountType::Child).await;
    app.server.clear_cookies();
    let res = app.server.get("/api/hidden").await;
    assert_eq!(res.status_code(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn hide_list_round_trip() {
    let (app, _) = boot_with_parent_and_child(AccountType::Child).await;

    // Initially empty.
    let res = app.server.get("/api/hidden").await;
    assert_eq!(res.status_code(), StatusCode::OK);
    let body: serde_json::Value = res.json();
    assert!(body.as_array().unwrap().is_empty());

    // Hide one.
    let res = app
        .server
        .post("/api/hidden")
        .json(&json!({
            "video_id": "vid-1",
            "video_title": "Test Title",
            "channel_id": "ch-1",
            "channel_title": "Test Channel",
            "video_thumbnail_url": "https://example.com/t.jpg",
            "duration_seconds": 120,
        }))
        .await;
    assert_eq!(res.status_code(), StatusCode::OK);
    let row: serde_json::Value = res.json();
    assert_eq!(row["video_id"], "vid-1");
    assert_eq!(row["video_title"], "Test Title");

    // List shows it.
    let res = app.server.get("/api/hidden").await;
    let body: serde_json::Value = res.json();
    assert_eq!(body.as_array().unwrap().len(), 1);
    assert_eq!(body[0]["video_id"], "vid-1");

    // Re-hiding is idempotent and refreshes metadata.
    let res = app
        .server
        .post("/api/hidden")
        .json(&json!({ "video_id": "vid-1", "video_title": "Renamed" }))
        .await;
    assert_eq!(res.status_code(), StatusCode::OK);

    let res = app.server.get("/api/hidden").await;
    let body: serde_json::Value = res.json();
    assert_eq!(body.as_array().unwrap().len(), 1);
    assert_eq!(body[0]["video_title"], "Renamed");

    // Delete it.
    let res = app.server.delete("/api/hidden/vid-1").await;
    assert_eq!(res.status_code(), StatusCode::NO_CONTENT);

    let res = app.server.get("/api/hidden").await;
    let body: serde_json::Value = res.json();
    assert!(body.as_array().unwrap().is_empty());
}

#[tokio::test]
async fn hide_is_per_child_isolated() {
    // Boot with one parent and one child, then add a second child and
    // mint a session for them.
    let (mut app, _auth_a) = boot_with_parent_and_child(AccountType::Child).await;
    let child_a = app.child_id.unwrap();
    let child_b = insert_account(&app.pool, "Child Two", AccountType::Child).await;

    // Child A hides vid-shared.
    let res = app
        .server
        .post("/api/hidden")
        .json(&json!({ "video_id": "vid-shared" }))
        .await;
    assert_eq!(res.status_code(), StatusCode::OK);

    // Switch session to child B by clearing cookies and adding B's cookie.
    app.server.clear_cookies();
    let auth_b = mint_session_cookie(&app, child_b).await;
    app.server
        .add_cookie(Cookie::new(auth_b.name, auth_b.value));

    // Child B sees no hidden videos.
    let res = app.server.get("/api/hidden").await;
    let body: serde_json::Value = res.json();
    assert!(body.as_array().unwrap().is_empty());

    // is_hidden_for_child confirms isolation.
    assert!(is_hidden_for_child(&app.pool, child_a, "vid-shared")
        .await
        .unwrap());
    assert!(!is_hidden_for_child(&app.pool, child_b, "vid-shared")
        .await
        .unwrap());
}

#[tokio::test]
async fn can_child_view_denies_hidden_even_if_allowlisted() {
    let (app, _auth) = boot_with_parent_and_child(AccountType::Child).await;
    let child_id = app.child_id.unwrap();
    let parent_id = app.parent_id.unwrap();

    // Allowlist the video first.
    sqlx::query(
        "INSERT INTO allowlisted_videos (child_account_id, video_id, video_title, added_by) \
         VALUES (?, 'vid-allow', 'Title', ?)",
    )
    .bind(child_id)
    .bind(parent_id)
    .execute(&app.pool)
    .await
    .unwrap();

    // Pre-hide check: visible.
    assert!(can_child_view(&app.pool, child_id, "vid-allow", None, &[])
        .await
        .unwrap());

    // Hide it.
    let res = app
        .server
        .post("/api/hidden")
        .json(&json!({ "video_id": "vid-allow" }))
        .await;
    assert_eq!(res.status_code(), StatusCode::OK);

    // Now denied.
    assert!(!can_child_view(&app.pool, child_id, "vid-allow", None, &[])
        .await
        .unwrap());

    // Unhide → visible again.
    let res = app.server.delete("/api/hidden/vid-allow").await;
    assert_eq!(res.status_code(), StatusCode::NO_CONTENT);
    assert!(can_child_view(&app.pool, child_id, "vid-allow", None, &[])
        .await
        .unwrap());
}

#[tokio::test]
async fn parent_cannot_use_hidden_routes() {
    let (app, _) = boot_setup_complete(AccountType::Parent).await;
    let res = app.server.get("/api/hidden").await;
    // child-only sub-router rejects parents.
    assert_eq!(res.status_code(), StatusCode::FORBIDDEN);
}
