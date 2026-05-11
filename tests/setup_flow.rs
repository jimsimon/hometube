//! Setup-wizard API + setup-redirect middleware behaviour.
//!
//! The wizard exposes two read endpoints (`status` + the PUT/POST
//! mutators) plus the setup-redirect middleware that bounces any
//! anonymous browser navigation to `/setup` until installation is
//! complete. This file covers the negative paths (empty bodies,
//! missing prerequisites) without invoking the discovery probe inside
//! `validate_credentials` — that probe makes a live HTTPS call to
//! `accounts.google.com` which is intentionally untested in unit-mode.

mod common;

use axum::http::StatusCode;
use common::boot;

#[tokio::test]
async fn fresh_app_setup_status_is_blank() {
    let app = boot().await;
    let res = app.server.get("/api/setup/status").await;
    assert_eq!(res.status_code(), StatusCode::OK);
    let body: serde_json::Value = res.json();
    assert_eq!(body["complete"], false);
    assert_eq!(body["has_credentials"], false);
    assert_eq!(body["has_first_parent"], false);
}

#[tokio::test]
async fn anonymous_root_redirects_to_setup() {
    let app = boot().await;
    let res = app.server.get("/").await;
    // setup_redirect middleware emits Redirect::to → 303 See Other.
    assert_eq!(res.status_code(), StatusCode::SEE_OTHER);
    let location = res.headers().get("location").expect("location header");
    assert_eq!(location, "/setup");
}

#[tokio::test]
async fn save_credentials_with_empty_body_is_400() {
    let app = boot().await;
    let res = app
        .server
        .post("/api/setup/credentials")
        .json(&serde_json::json!({}))
        .await;
    // Empty body fails JSON deserialisation (missing required fields)
    // → axum returns 422 Unprocessable Entity for that. Either 400 or
    // 422 is "client mistake" so we accept both.
    let status = res.status_code();
    assert!(
        status == StatusCode::BAD_REQUEST || status == StatusCode::UNPROCESSABLE_ENTITY,
        "expected 400/422, got {status}"
    );
}

#[tokio::test]
async fn save_credentials_with_blank_fields_is_400() {
    let app = boot().await;
    let res = app
        .server
        .post("/api/setup/credentials")
        .json(&serde_json::json!({
            "google_client_id": "",
            "google_client_secret": "",
            "youtube_api_key": "",
            "redirect_uri": "http://localhost"
        }))
        .await;
    assert_eq!(res.status_code(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn complete_is_rejected_before_prerequisites_met() {
    let app = boot().await;
    let res = app.server.post("/api/setup/complete").await;
    // No credentials, no parent → 400 with a helpful message.
    assert_eq!(res.status_code(), StatusCode::BAD_REQUEST);

    // Seed credentials but still no parent → still 400.
    common::seed_credentials(&app.pool).await;
    let res = app.server.post("/api/setup/complete").await;
    assert_eq!(res.status_code(), StatusCode::BAD_REQUEST);

    // Add a parent → now complete should succeed.
    common::insert_account(
        &app.pool,
        "google-parent-1",
        "p@example.test",
        "P",
        hometube::models::account::AccountType::Parent,
    )
    .await;
    let res = app.server.post("/api/setup/complete").await;
    assert_eq!(res.status_code(), StatusCode::NO_CONTENT);

    // Status now flips to complete.
    let res = app.server.get("/api/setup/status").await;
    let body: serde_json::Value = res.json();
    assert_eq!(body["complete"], true);
    assert_eq!(body["has_credentials"], true);
    assert_eq!(body["has_first_parent"], true);
}

#[tokio::test]
async fn save_credentials_with_invalid_redirect_uri_is_400() {
    let app = boot().await;
    let res = app
        .server
        .post("/api/setup/credentials")
        .json(&serde_json::json!({
            "google_client_id": "id",
            "google_client_secret": "secret",
            "youtube_api_key": "key",
            "redirect_uri": "ftp://no",
        }))
        .await;
    assert_eq!(res.status_code(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn anonymous_api_returns_503_during_setup() {
    let app = boot().await;
    // Any non-allowlisted API path while setup is incomplete returns
    // 503 with a hint to visit /setup.
    let res = app.server.get("/api/cron/jobs").await;
    assert_eq!(res.status_code(), StatusCode::SERVICE_UNAVAILABLE);
}
