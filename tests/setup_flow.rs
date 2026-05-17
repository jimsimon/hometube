//! Setup-wizard API + setup-redirect middleware behaviour.
//!
//! The wizard exposes a read endpoint (`status`) plus the `complete`
//! mutator, and the setup-redirect middleware bounces any anonymous
//! browser navigation to `/setup` until installation is complete.

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
async fn complete_is_rejected_before_prerequisites_met() {
    let app = boot().await;
    let res = app.server.post("/api/setup/complete").await;
    // No parent → 400 with a helpful message.
    assert_eq!(res.status_code(), StatusCode::BAD_REQUEST);

    // Add a parent → now complete should succeed.
    common::insert_account(
        &app.pool,
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
    assert_eq!(body["has_first_parent"], true);
}

#[tokio::test]
async fn anonymous_api_returns_503_during_setup() {
    let app = boot().await;
    // Any non-allowlisted API path while setup is incomplete returns
    // 503 with a hint to visit /setup.
    let res = app.server.get("/api/cron/jobs").await;
    assert_eq!(res.status_code(), StatusCode::SERVICE_UNAVAILABLE);
}
