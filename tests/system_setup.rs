//! System and setup route tests.

mod common;

use axum::http::StatusCode;
use common::{boot, boot_with_parent_and_child};
use hometube::models::account::AccountType;
use serde_json::json;

// ===========================================================================
// Setup
// ===========================================================================

#[tokio::test]
async fn setup_status_reflects_fresh_state() {
    let app = boot().await;
    let res = app.server.get("/api/setup/status").await;
    assert_eq!(res.status_code(), StatusCode::OK);
    let body: serde_json::Value = res.json();
    assert_eq!(body["complete"], false);
    assert_eq!(body["has_first_parent"], false);
}

#[tokio::test]
async fn setup_complete_rejects_without_prerequisites() {
    let app = boot().await;
    let res = app.server.post("/api/setup/complete").await;
    assert_eq!(res.status_code(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn complete_rejects_without_parent() {
    let app = boot().await;
    let res = app.server.post("/api/setup/complete").await;
    assert_eq!(res.status_code(), StatusCode::BAD_REQUEST);
    let body = res.text();
    assert!(body.contains("parent"));
}

// ===========================================================================
// System / yt-dlp
// ===========================================================================

#[tokio::test]
async fn system_ytdlp_returns_default_status() {
    let (app, _auth) = boot_with_parent_and_child(AccountType::Parent).await;

    // Seed ytdlp_info row.
    let cfg = hometube::config::Config::from_env().unwrap();
    hometube::services::cron::seed_ytdlp_info(&app.pool, &cfg)
        .await
        .unwrap();

    let res = app.server.get("/api/system/ytdlp").await;
    assert_eq!(res.status_code(), StatusCode::OK);
    let body: serde_json::Value = res.json();
    assert!(body.get("binary_path").is_some());
    assert!(body.get("current_version").is_some());
}

#[tokio::test]
async fn system_ytdlp_child_is_forbidden() {
    let (app, _auth) = boot_with_parent_and_child(AccountType::Child).await;
    let res = app.server.get("/api/system/ytdlp").await;
    assert_eq!(res.status_code(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn system_pot_server_returns_status() {
    let (app, _auth) = boot_with_parent_and_child(AccountType::Parent).await;
    let res = app.server.get("/api/system/pot-server").await;
    assert_eq!(res.status_code(), StatusCode::OK);
    let body: serde_json::Value = res.json();
    // The pot server won't be running in tests, so it should report
    // unavailable.
    assert_eq!(body["available"], false);
    assert!(body.get("url").is_some());
}

// ===========================================================================
// Cookies
// ===========================================================================

#[tokio::test]
async fn cookies_get_returns_not_configured_initially() {
    let (app, _auth) = boot_with_parent_and_child(AccountType::Parent).await;
    let res = app.server.get("/api/system/ytdlp/cookies").await;
    assert_eq!(res.status_code(), StatusCode::OK);
    let body: serde_json::Value = res.json();
    assert_eq!(body["configured"], false);
    assert!(body.get("line_count").is_none());
}

#[tokio::test]
async fn cookies_put_stores_and_returns_status() {
    let (app, _auth) = boot_with_parent_and_child(AccountType::Parent).await;
    let res = app
        .server
        .put("/api/system/ytdlp/cookies")
        .json(&json!({
            "content": "# Netscape HTTP Cookie File\n.youtube.com\tTRUE\t/\tFALSE\t0\tCOOKIE\tVALUE\n"
        }))
        .await;
    assert_eq!(res.status_code(), StatusCode::OK);
    let body: serde_json::Value = res.json();
    assert_eq!(body["configured"], true);
    assert_eq!(body["line_count"], 2);
}

#[tokio::test]
async fn cookies_get_reflects_saved_state() {
    let (app, _auth) = boot_with_parent_and_child(AccountType::Parent).await;
    app.server
        .put("/api/system/ytdlp/cookies")
        .json(&json!({
            "content": "# Netscape HTTP Cookie File\n.youtube.com\tTRUE\t/\tFALSE\t0\tA\tB\n.youtube.com\tTRUE\t/\tFALSE\t0\tC\tD\n"
        }))
        .await;

    let res = app.server.get("/api/system/ytdlp/cookies").await;
    assert_eq!(res.status_code(), StatusCode::OK);
    let body: serde_json::Value = res.json();
    assert_eq!(body["configured"], true);
    assert_eq!(body["line_count"], 3);
}

#[tokio::test]
async fn cookies_delete_removes_cookies() {
    let (app, _auth) = boot_with_parent_and_child(AccountType::Parent).await;
    app.server
        .put("/api/system/ytdlp/cookies")
        .json(&json!({ "content": "# cookies\n.youtube.com\tTRUE\t/\tFALSE\t0\tX\tY\n" }))
        .await;

    let res = app.server.delete("/api/system/ytdlp/cookies").await;
    assert_eq!(res.status_code(), StatusCode::OK);
    let body: serde_json::Value = res.json();
    assert_eq!(body["configured"], false);

    // Verify GET also reflects the removal.
    let res = app.server.get("/api/system/ytdlp/cookies").await;
    let body: serde_json::Value = res.json();
    assert_eq!(body["configured"], false);
}

#[tokio::test]
async fn cookies_put_rejects_empty_content() {
    let (app, _auth) = boot_with_parent_and_child(AccountType::Parent).await;
    let res = app
        .server
        .put("/api/system/ytdlp/cookies")
        .json(&json!({ "content": "   " }))
        .await;
    assert_eq!(res.status_code(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn cookies_put_rejects_oversized_content() {
    let (app, _auth) = boot_with_parent_and_child(AccountType::Parent).await;
    // 1 MB + 1 byte
    let oversized = "x".repeat(1_024 * 1_024 + 1);
    let res = app
        .server
        .put("/api/system/ytdlp/cookies")
        .json(&json!({ "content": oversized }))
        .await;
    assert_eq!(res.status_code(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn cookies_child_is_forbidden() {
    let (app, _auth) = boot_with_parent_and_child(AccountType::Child).await;
    let res = app.server.get("/api/system/ytdlp/cookies").await;
    assert_eq!(res.status_code(), StatusCode::FORBIDDEN);

    let res = app
        .server
        .put("/api/system/ytdlp/cookies")
        .json(&json!({ "content": "cookie data" }))
        .await;
    assert_eq!(res.status_code(), StatusCode::FORBIDDEN);

    let res = app.server.delete("/api/system/ytdlp/cookies").await;
    assert_eq!(res.status_code(), StatusCode::FORBIDDEN);
}
