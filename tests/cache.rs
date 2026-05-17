//! Cache-management API.
//!
//! Coverage:
//! - empty stats on a fresh DB
//! - default settings (read + write round-trip)
//! - validation of the `max_size` preset string
//! - clear-all path

mod common;

use axum::http::StatusCode;
use common::boot_with_parent_and_child;
use hometube::models::account::AccountType;
use serde_json::json;

#[tokio::test]
async fn fresh_stats_are_zero() {
    let (app, _auth) = boot_with_parent_and_child(AccountType::Parent).await;
    let res = app.server.get("/api/cache/stats").await;
    assert_eq!(res.status_code(), StatusCode::OK);
    let body: serde_json::Value = res.json();
    assert_eq!(body["total_bytes"], 0);
    assert_eq!(body["segment_count"], 0);
    assert_eq!(body["video_count"], 0);
    // The `top_videos` array exists even when empty.
    assert!(body["top_videos"].is_array());
}

#[tokio::test]
async fn settings_have_defaults() {
    let (app, _auth) = boot_with_parent_and_child(AccountType::Parent).await;
    let res = app.server.get("/api/cache/settings").await;
    assert_eq!(res.status_code(), StatusCode::OK);
    let body: serde_json::Value = res.json();
    // From `services::video_cache::DEFAULT_CACHE_MAX_SIZE` and
    // `DEFAULT_TTL_HOURS`.
    assert_eq!(body["max_size"], "100 GB");
    // The harness `boot()` doesn't touch metadata_cache_ttl_hours, so
    // it falls back to the default.
    assert_eq!(body["metadata_ttl_hours"], 4);
}

#[tokio::test]
async fn put_settings_persists_known_preset() {
    let (app, _auth) = boot_with_parent_and_child(AccountType::Parent).await;
    let res = app
        .server
        .put("/api/cache/settings")
        .json(&json!({ "max_size": "10 GB", "metadata_ttl_hours": 4 }))
        .await;
    assert_eq!(res.status_code(), StatusCode::OK);
    let body: serde_json::Value = res.json();
    assert_eq!(body["max_size"], "10 GB");

    // Round-trip via the GET endpoint.
    let res = app.server.get("/api/cache/settings").await;
    let body: serde_json::Value = res.json();
    assert_eq!(body["max_size"], "10 GB");
}

#[tokio::test]
async fn put_settings_rejects_unknown_preset() {
    let (app, _auth) = boot_with_parent_and_child(AccountType::Parent).await;
    let res = app
        .server
        .put("/api/cache/settings")
        .json(&json!({ "max_size": "weird" }))
        .await;
    assert_eq!(res.status_code(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn put_settings_rejects_out_of_range_ttl() {
    let (app, _auth) = boot_with_parent_and_child(AccountType::Parent).await;
    let res = app
        .server
        .put("/api/cache/settings")
        .json(&json!({ "metadata_ttl_hours": 9999 }))
        .await;
    assert_eq!(res.status_code(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn list_videos_is_initially_empty() {
    let (app, _auth) = boot_with_parent_and_child(AccountType::Parent).await;
    let res = app.server.get("/api/cache/videos").await;
    let body: serde_json::Value = res.json();
    assert!(body.as_array().unwrap().is_empty());
}

#[tokio::test]
async fn clear_all_returns_no_content() {
    let (app, _auth) = boot_with_parent_and_child(AccountType::Parent).await;
    let res = app.server.post("/api/cache/clear").await;
    assert_eq!(res.status_code(), StatusCode::NO_CONTENT);
}
