//! System and setup route tests.

mod common;

use axum::http::StatusCode;
use common::{boot, boot_with_parent_and_child, seed_credentials};
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
    assert_eq!(body["has_credentials"], false);
    assert_eq!(body["has_first_parent"], false);
}

#[tokio::test]
async fn setup_status_after_credentials() {
    let app = boot().await;
    seed_credentials(&app.pool).await;
    let res = app.server.get("/api/setup/status").await;
    let body: serde_json::Value = res.json();
    assert_eq!(body["has_credentials"], true);
    assert_eq!(body["has_first_parent"], false);
}

#[tokio::test]
async fn setup_complete_rejects_without_prerequisites() {
    let app = boot().await;
    let res = app.server.post("/api/setup/complete").await;
    assert_eq!(res.status_code(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn save_credentials_rejects_empty_client_id() {
    let app = boot().await;
    let res = app
        .server
        .post("/api/setup/credentials")
        .json(&json!({
            "google_client_id": "",
            "google_client_secret": "sec",
            "youtube_api_key": "key",
            "redirect_uri": "http://localhost:3000/api/auth/callback"
        }))
        .await;
    assert_eq!(res.status_code(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn save_credentials_rejects_empty_secret() {
    let app = boot().await;
    let res = app
        .server
        .post("/api/setup/credentials")
        .json(&json!({
            "google_client_id": "id",
            "google_client_secret": "   ",
            "youtube_api_key": "key",
            "redirect_uri": "http://localhost:3000/api/auth/callback"
        }))
        .await;
    assert_eq!(res.status_code(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn save_credentials_rejects_empty_api_key() {
    let app = boot().await;
    let res = app
        .server
        .post("/api/setup/credentials")
        .json(&json!({
            "google_client_id": "id",
            "google_client_secret": "sec",
            "youtube_api_key": "",
            "redirect_uri": "http://localhost:3000/api/auth/callback"
        }))
        .await;
    assert_eq!(res.status_code(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn save_credentials_rejects_bad_redirect_uri() {
    let app = boot().await;
    let res = app
        .server
        .post("/api/setup/credentials")
        .json(&json!({
            "google_client_id": "id",
            "google_client_secret": "sec",
            "youtube_api_key": "key",
            "redirect_uri": "ftp://bad"
        }))
        .await;
    assert_eq!(res.status_code(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn test_credentials_rejects_same_validation() {
    let app = boot().await;
    let res = app
        .server
        .post("/api/setup/test-credentials")
        .json(&json!({
            "google_client_id": "id",
            "google_client_secret": "",
            "youtube_api_key": "key",
            "redirect_uri": "http://localhost:3000/cb"
        }))
        .await;
    assert_eq!(res.status_code(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn complete_rejects_without_parent() {
    let app = boot().await;
    seed_credentials(&app.pool).await;
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
