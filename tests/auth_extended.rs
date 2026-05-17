//! Extended auth route tests — covers PIN validation edge cases, profile
//! listing with multiple accounts, switch behaviour, and cookie handling.

mod common;

use axum::http::StatusCode;
use common::{
    boot_setup_complete, boot_with_parent_and_child, insert_account, mint_session_cookie,
};
use hometube::models::account::AccountType;
use serde_json::json;
use tower_cookies::cookie::Cookie;

// ---------------------------------------------------------------------------
// Profiles endpoint
// ---------------------------------------------------------------------------

#[tokio::test]
async fn profiles_lists_multiple_accounts() {
    let (app, _auth) = boot_with_parent_and_child(AccountType::Parent).await;

    // Add more accounts.
    insert_account(&app.pool, "Child Two", AccountType::Child).await;

    let res = app.server.get("/api/auth/profiles").await;
    assert_eq!(res.status_code(), StatusCode::OK);
    let body: serde_json::Value = res.json();
    let arr = body.as_array().unwrap();
    // parent + child1 + child2 = 3
    assert_eq!(arr.len(), 3);
}

#[tokio::test]
async fn profiles_does_not_expose_tokens() {
    let (app, _auth) = boot_setup_complete(AccountType::Parent).await;

    let res = app.server.get("/api/auth/profiles").await;
    let body: serde_json::Value = res.json();
    let first = &body[0];
    assert!(first.get("access_token").is_none());
    assert!(first.get("refresh_token").is_none());
}

// ---------------------------------------------------------------------------
// Me endpoint
// ---------------------------------------------------------------------------

#[tokio::test]
async fn me_returns_account_info() {
    let (app, _auth) = boot_setup_complete(AccountType::Parent).await;

    let res = app.server.get("/api/auth/me").await;
    assert_eq!(res.status_code(), StatusCode::OK);
    let body: serde_json::Value = res.json();
    assert_eq!(body["account_type"], "parent");
    assert_eq!(body["display_name"], "Parent One");
}

// ---------------------------------------------------------------------------
// Switch endpoint
// ---------------------------------------------------------------------------

#[tokio::test]
async fn switch_to_child_from_parent_works() {
    let (app, _auth) = boot_with_parent_and_child(AccountType::Parent).await;
    let child_id = app.child_id.unwrap();

    let res = app
        .server
        .post("/api/auth/switch")
        .json(&json!({ "account_id": child_id }))
        .await;
    assert_eq!(res.status_code(), StatusCode::OK);
}

#[tokio::test]
async fn switch_to_parent_requires_pin() {
    let (app, _auth) = boot_with_parent_and_child(AccountType::Child).await;
    let parent_id = app.parent_id.unwrap();

    // Set a PIN on the parent account.
    let parent_auth = mint_session_cookie(&app, parent_id).await;
    app.server
        .put("/api/auth/pin")
        .clear_cookies()
        .add_cookie(Cookie::new(parent_auth.name, parent_auth.value))
        .json(&json!({ "pin": "1234" }))
        .await;

    // Now try switching to parent from child without PIN.
    let res = app
        .server
        .post("/api/auth/switch")
        .json(&json!({ "account_id": parent_id }))
        .await;
    // Should require PIN.
    assert_eq!(res.status_code(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn switch_to_parent_with_correct_pin() {
    let (app, _auth) = boot_with_parent_and_child(AccountType::Child).await;
    let parent_id = app.parent_id.unwrap();

    // Set a PIN.
    let parent_auth = mint_session_cookie(&app, parent_id).await;
    app.server
        .put("/api/auth/pin")
        .clear_cookies()
        .add_cookie(Cookie::new(parent_auth.name, parent_auth.value.clone()))
        .json(&json!({ "pin": "5678" }))
        .await;

    // Switch with correct PIN.
    let res = app
        .server
        .post("/api/auth/switch")
        .json(&json!({ "account_id": parent_id, "pin": "5678" }))
        .await;
    assert_eq!(res.status_code(), StatusCode::OK);
}

#[tokio::test]
async fn switch_to_parent_with_wrong_pin() {
    let (app, _auth) = boot_with_parent_and_child(AccountType::Child).await;
    let parent_id = app.parent_id.unwrap();

    // Set a PIN.
    let parent_auth = mint_session_cookie(&app, parent_id).await;
    app.server
        .put("/api/auth/pin")
        .clear_cookies()
        .add_cookie(Cookie::new(parent_auth.name, parent_auth.value.clone()))
        .json(&json!({ "pin": "1111" }))
        .await;

    // Switch with wrong PIN.
    let res = app
        .server
        .post("/api/auth/switch")
        .json(&json!({ "account_id": parent_id, "pin": "9999" }))
        .await;
    assert_eq!(res.status_code(), StatusCode::FORBIDDEN);
}

// ---------------------------------------------------------------------------
// PIN management
// ---------------------------------------------------------------------------

#[tokio::test]
async fn set_pin_too_short_is_rejected() {
    let (app, _auth) = boot_setup_complete(AccountType::Parent).await;

    let res = app
        .server
        .put("/api/auth/pin")
        .json(&json!({ "pin": "12" }))
        .await;
    assert_eq!(res.status_code(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn set_pin_too_long_is_rejected() {
    let (app, _auth) = boot_setup_complete(AccountType::Parent).await;

    let res = app
        .server
        .put("/api/auth/pin")
        .json(&json!({ "pin": "12345678901" }))
        .await;
    assert_eq!(res.status_code(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn set_pin_with_non_digits_is_rejected() {
    let (app, _auth) = boot_setup_complete(AccountType::Parent).await;

    let res = app
        .server
        .put("/api/auth/pin")
        .json(&json!({ "pin": "12ab" }))
        .await;
    assert_eq!(res.status_code(), StatusCode::BAD_REQUEST);
}

// ---------------------------------------------------------------------------
// Logout
// ---------------------------------------------------------------------------

#[tokio::test]
async fn logout_clears_session() {
    let (app, _auth) = boot_setup_complete(AccountType::Parent).await;

    let res = app.server.post("/api/auth/logout").await;
    // Logout redirects.
    let status = res.status_code().as_u16();
    assert!(status == 200 || (300..400).contains(&status));
}

// ---------------------------------------------------------------------------
// Login redirect
// ---------------------------------------------------------------------------

// OAuth login test removed — Google OAuth has been replaced by PIN-based auth.
