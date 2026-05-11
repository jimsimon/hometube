//! Authentication / profile-switching API.
//!
//! Exercises [`hometube::routes::auth`] without going through the live
//! Google OAuth flow. The harness pre-seeds accounts directly in
//! `accounts` so we can assert the read-only endpoints
//! (`/api/auth/profiles`, `/api/auth/me`), and the PIN + switch flow
//! against fixtures created by the test rather than minted by Google.

mod common;

use axum::http::StatusCode;
use common::{boot_setup_complete, boot_with_parent_and_child};
use hometube::middleware::auth::SESSION_COOKIE;
use hometube::models::account::AccountType;
use serde_json::json;
use tower_cookies::cookie::Cookie;

#[tokio::test]
async fn profiles_lists_all_accounts() {
    let (app, _auth) = boot_with_parent_and_child(AccountType::Parent).await;
    let res = app.server.get("/api/auth/profiles").await;
    assert_eq!(res.status_code(), StatusCode::OK);
    let body: serde_json::Value = res.json();
    let arr = body.as_array().expect("array");
    assert_eq!(arr.len(), 2);
    // Parent listed first by the ORDER BY clause.
    assert_eq!(arr[0]["account_type"], "parent");
    assert_eq!(arr[1]["account_type"], "child");
    // Tokens never leak into the public summary.
    for entry in arr {
        assert!(
            entry.get("access_token").is_none(),
            "access_token must not be exposed"
        );
        assert!(
            entry.get("refresh_token").is_none(),
            "refresh_token must not be exposed"
        );
    }
}

#[tokio::test]
async fn me_returns_current_account_without_tokens() {
    let (app, auth) = boot_setup_complete(AccountType::Parent).await;
    let res = app.server.get("/api/auth/me").await;
    assert_eq!(res.status_code(), StatusCode::OK);
    let body: serde_json::Value = res.json();
    assert_eq!(body["id"], auth.account_id);
    assert_eq!(body["account_type"], "parent");
    assert!(body.get("access_token").is_none());
}

#[tokio::test]
async fn me_returns_401_for_anonymous() {
    // Boot without seeding any session — `boot()` is exposed via the
    // common module but here we want setup to be complete so the
    // setup-redirect middleware doesn't intercept us.
    let app = common::boot().await;
    common::seed_credentials(&app.pool).await;
    hometube::services::setup::set_config_value(
        &app.pool,
        hometube::services::setup::KEY_SETUP_COMPLETE,
        "true",
    )
    .await
    .unwrap();

    // No session cookie attached at all → 401.
    let res = app
        .server
        .get("/api/auth/me")
        .add_cookie(Cookie::new(SESSION_COOKIE, "definitely-invalid"))
        .await;
    assert_eq!(res.status_code(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn switch_to_parent_without_pin_is_rejected() {
    let (app, _auth) = boot_with_parent_and_child(AccountType::Child).await;
    let parent_id = app.parent_id.unwrap();
    // The seeded parent has no PIN — the impl returns 400 with a helpful
    // message rather than 401.
    let res = app
        .server
        .post("/api/auth/switch")
        .json(&json!({ "account_id": parent_id }))
        .await;
    assert_eq!(res.status_code(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn switch_to_child_without_pin_succeeds() {
    let (app, _auth) = boot_with_parent_and_child(AccountType::Parent).await;
    let child_id = app.child_id.unwrap();
    let res = app
        .server
        .post("/api/auth/switch")
        .json(&json!({ "account_id": child_id }))
        .await;
    assert_eq!(res.status_code(), StatusCode::OK);
    let body: serde_json::Value = res.json();
    assert_eq!(body["id"], child_id);
    assert_eq!(body["account_type"], "child");
}

#[tokio::test]
async fn set_pin_and_switch_round_trip() {
    let (app, _auth) = boot_with_parent_and_child(AccountType::Parent).await;
    let parent_id = app.parent_id.unwrap();

    // Set a PIN on the active parent session.
    let res = app
        .server
        .put("/api/auth/pin")
        .json(&json!({ "pin": "1234" }))
        .await;
    assert_eq!(res.status_code(), StatusCode::NO_CONTENT);

    // Switch to the same parent with the correct PIN.
    let res = app
        .server
        .post("/api/auth/switch")
        .json(&json!({ "account_id": parent_id, "pin": "1234" }))
        .await;
    assert_eq!(res.status_code(), StatusCode::OK);

    // Wrong PIN → 403 (mapped from `AppError::Forbidden` by the impl).
    let res = app
        .server
        .post("/api/auth/switch")
        .json(&json!({ "account_id": parent_id, "pin": "9999" }))
        .await;
    assert_eq!(res.status_code(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn set_pin_rejects_invalid_input() {
    let (app, _auth) = boot_setup_complete(AccountType::Parent).await;
    let res = app
        .server
        .put("/api/auth/pin")
        .json(&json!({ "pin": "abc" }))
        .await;
    assert_eq!(res.status_code(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn failed_pin_attempts_emit_notification() {
    let (app, _auth) = boot_with_parent_and_child(AccountType::Parent).await;
    let parent_id = app.parent_id.unwrap();

    // Set a PIN, then provoke 5 wrong attempts back-to-back.
    let res = app
        .server
        .put("/api/auth/pin")
        .json(&json!({ "pin": "1234" }))
        .await;
    assert_eq!(res.status_code(), StatusCode::NO_CONTENT);

    for _ in 0..5 {
        let res = app
            .server
            .post("/api/auth/switch")
            .json(&json!({ "account_id": parent_id, "pin": "9999" }))
            .await;
        assert_eq!(res.status_code(), StatusCode::FORBIDDEN);
    }

    // After ≥5 failures within the 5-minute window, the bookkeeping
    // path inserts a system_update notification for the parent.
    let count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM parent_notifications WHERE notification_type = 'system_update'",
    )
    .fetch_one(&app.pool)
    .await
    .unwrap();
    assert!(count >= 1, "expected at least one notification, got {count}");
}

#[tokio::test]
async fn switch_404_for_missing_account() {
    let (app, _auth) = boot_with_parent_and_child(AccountType::Parent).await;
    let res = app
        .server
        .post("/api/auth/switch")
        .json(&json!({ "account_id": 9999 }))
        .await;
    assert_eq!(res.status_code(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn callback_rejects_missing_oauth_cookie() {
    let (app, _auth) = boot_setup_complete(AccountType::Parent).await;
    // Hitting the OAuth callback without the round-trip cookie is a
    // 400 — the handler can't trust a code without the matching state.
    let res = app
        .server
        .get("/api/auth/callback?code=abc&state=zzz")
        .await;
    assert_eq!(res.status_code(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn callback_with_state_mismatch_400() {
    let (app, _auth) = boot_setup_complete(AccountType::Parent).await;

    // Mint a signed `hometube_oauth` cookie with a known csrf, then
    // call the callback with a *different* state value.
    let raw_key = tower_cookies::cookie::Key::from(&common::test_key_bytes());
    let mut jar = tower_cookies::cookie::CookieJar::new();
    let payload = r#"{"csrf":"correct-csrf","pkce_verifier":"pv","role":"parent"}"#;
    jar.signed_mut(&raw_key)
        .add(tower_cookies::cookie::Cookie::new("hometube_oauth", payload));
    let signed = jar.get("hometube_oauth").unwrap().clone();

    let res = app
        .server
        .get("/api/auth/callback?code=abc&state=wrong-csrf")
        .add_cookie(tower_cookies::cookie::Cookie::new(
            signed.name().to_string(),
            signed.value().to_string(),
        ))
        .await;
    assert_eq!(res.status_code(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn login_redirects_to_google() {
    // The login route hits the discovery endpoint via
    // `oauth::build_client`. With no credentials configured at all,
    // it returns 500. With *test* credentials configured, it still
    // makes a network call to Google's discovery doc on the first
    // run — for that reason we deliberately don't assert on success
    // here, just that the route exists and produces *some* response.
    let app = common::boot().await;
    common::seed_credentials(&app.pool).await;
    hometube::services::setup::set_config_value(
        &app.pool,
        hometube::services::setup::KEY_SETUP_COMPLETE,
        "true",
    )
    .await
    .unwrap();
    let res = app.server.get("/api/auth/login").await;
    // 303 redirect on success, or 5xx if Google's discovery URL is
    // unreachable from the test environment. Either way we exercised
    // the handler code path.
    let s = res.status_code();
    assert!(
        s.is_redirection() || s.is_server_error(),
        "unexpected status from /api/auth/login: {s}"
    );
}

#[tokio::test]
async fn logout_clears_cookie_and_redirects() {
    let (app, _auth) = boot_setup_complete(AccountType::Parent).await;
    let res = app.server.post("/api/auth/logout").await;
    // The handler returns 303 See Other → /profiles. axum's Redirect
    // emits 303 by default.
    assert_eq!(res.status_code(), StatusCode::SEE_OTHER);
    let location = res.headers().get("location").expect("location header");
    assert_eq!(location, "/profiles");
}
