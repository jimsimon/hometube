//! Role-gating: parent-only endpoints reject children + anonymous users.
//!
//! The router applies `require_parent` as a `route_layer` over a sub-router
//! so the gate fires before any handler logic. We hit a representative
//! sample of routes from each functional area (cron, cache, family,
//! family-playlists, child usage stats, parent notifications) and assert
//! the cross-product of:
//!
//! - parent session → 200 (or 200-ish: 200/201/204)
//! - child session  → 403
//! - no session     → 401

mod common;

use axum::http::StatusCode;
use common::{boot_with_parent_and_child, mint_session_cookie};
use hometube::middleware::auth::SESSION_COOKIE;
use hometube::models::account::AccountType;
use serde_json::json;
use tower_cookies::cookie::Cookie;

/// Endpoints we expect to behave the same way (gated by `require_parent`).
const READ_ENDPOINTS: &[&str] = &[
    "/api/cron/jobs",
    "/api/cache/stats",
    "/api/cache/settings",
    "/api/cache/videos",
    "/api/family/members",
    "/api/notifications",
    "/api/notifications/unread-count",
    "/api/accounts",
];

#[tokio::test]
async fn parent_session_can_read_parent_only_routes() {
    let (app, _auth) = boot_with_parent_and_child(AccountType::Parent).await;
    for path in READ_ENDPOINTS {
        let res = app.server.get(path).await;
        assert!(
            res.status_code().is_success(),
            "GET {path} should succeed for a parent (got {})",
            res.status_code()
        );
    }
}

#[tokio::test]
async fn child_session_is_forbidden_from_parent_only_routes() {
    let (app, _auth) = boot_with_parent_and_child(AccountType::Child).await;
    for path in READ_ENDPOINTS {
        let res = app.server.get(path).await;
        assert_eq!(
            res.status_code(),
            StatusCode::FORBIDDEN,
            "GET {path} should 403 for a child (got {})",
            res.status_code()
        );
    }
}

#[tokio::test]
async fn anonymous_is_unauthorized_on_parent_only_routes() {
    let (app, _auth) = boot_with_parent_and_child(AccountType::Parent).await;
    // Re-issue requests without the harness's session cookie. We can't
    // remove cookies via the public TestServer API on a non-mutable
    // borrow, so we issue per-request cookies pointing at a junk value.
    let bad = Cookie::new(SESSION_COOKIE, "not-a-real-session");
    for path in READ_ENDPOINTS {
        let res = app.server.get(path).clear_cookies().add_cookie(bad.clone()).await;
        assert_eq!(
            res.status_code(),
            StatusCode::UNAUTHORIZED,
            "GET {path} should 401 anonymously (got {})",
            res.status_code()
        );
    }
}

#[tokio::test]
async fn child_post_to_family_playlists_is_403() {
    let (app, _auth) = boot_with_parent_and_child(AccountType::Child).await;
    let res = app
        .server
        .post("/api/family-playlists")
        .json(&json!({ "title": "Hi", "description": "" }))
        .await;
    // Family-playlists POST is parent-gated inside the handler, not via
    // the `require_parent` layer (the route is shared with children).
    // The handler returns 403 for non-parents.
    assert_eq!(res.status_code(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn parent_can_read_per_child_endpoints() {
    let (app, _auth) = boot_with_parent_and_child(AccountType::Parent).await;
    let child_id = app.child_id.unwrap();

    let res = app
        .server
        .get(&format!("/api/children/{child_id}/usage-stats"))
        .await;
    assert!(res.status_code().is_success());

    let res = app
        .server
        .get(&format!("/api/children/{child_id}/settings"))
        .await;
    assert!(res.status_code().is_success());
}

#[tokio::test]
async fn child_cannot_read_per_child_endpoints() {
    let (app, _auth) = boot_with_parent_and_child(AccountType::Child).await;
    let child_id = app.child_id.unwrap();
    // Same child trying to read its own /api/children/:id/* → 403
    // because the layer requires *parent* role, even for own data.
    let res = app
        .server
        .get(&format!("/api/children/{child_id}/usage-stats"))
        .await;
    assert_eq!(res.status_code(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn switching_role_changes_authorization() {
    // Sanity check: the same TestServer can be made to "log in" as
    // different accounts by minting a fresh signed cookie. This guards
    // against any session caching that would let a child re-use a
    // parent session.
    let (app, _parent_auth) = boot_with_parent_and_child(AccountType::Parent).await;
    // Parent session → cron list works.
    let res = app.server.get("/api/cron/jobs").await;
    assert!(res.status_code().is_success());

    // Re-mint as the child, override the cookie.
    let child_id = app.child_id.unwrap();
    let auth = mint_session_cookie(&app, child_id).await;
    let res = app
        .server
        .get("/api/cron/jobs")
        .clear_cookies()
        .add_cookie(Cookie::new(auth.name, auth.value))
        .await;
    assert_eq!(res.status_code(), StatusCode::FORBIDDEN);
}
