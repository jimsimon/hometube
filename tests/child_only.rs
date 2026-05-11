//! Mirror of `parent_only.rs` for the child-only sub-router.
//!
//! Routes scoped through `require_child` (feed, heartbeat, search,
//! channels, subscriptions, playlists, likes, bookmarks, sleep timer,
//! downloads) all reject parent sessions with 403.

mod common;

use axum::http::StatusCode;
use common::boot_with_parent_and_child;
use hometube::middleware::auth::SESSION_COOKIE;
use hometube::models::account::AccountType;
use serde_json::json;
use tower_cookies::cookie::Cookie;

const CHILD_GET_ENDPOINTS: &[&str] = &[
    "/api/feed/continue-watching",
    "/api/feed/new-videos",
    "/api/subscriptions",
    "/api/playlists",
    "/api/likes",
    "/api/bookmarks",
    "/api/timer",
    "/api/downloads",
    "/api/children/me/settings",
];

#[tokio::test]
async fn child_session_can_read_child_only_routes() {
    let (app, _auth) = boot_with_parent_and_child(AccountType::Child).await;
    for path in CHILD_GET_ENDPOINTS {
        let res = app.server.get(path).await;
        assert!(
            res.status_code().is_success(),
            "GET {path} should succeed for a child (got {})",
            res.status_code()
        );
    }
}

#[tokio::test]
async fn parent_session_is_forbidden_from_child_only_routes() {
    let (app, _auth) = boot_with_parent_and_child(AccountType::Parent).await;
    for path in CHILD_GET_ENDPOINTS {
        let res = app.server.get(path).await;
        assert_eq!(
            res.status_code(),
            StatusCode::FORBIDDEN,
            "GET {path} should 403 for a parent (got {})",
            res.status_code()
        );
    }
}

#[tokio::test]
async fn anonymous_child_routes_are_401() {
    let (app, _auth) = boot_with_parent_and_child(AccountType::Child).await;
    let bad = Cookie::new(SESSION_COOKIE, "junk");
    for path in CHILD_GET_ENDPOINTS {
        let res = app
            .server
            .get(path)
            .clear_cookies()
            .add_cookie(bad.clone())
            .await;
        assert_eq!(
            res.status_code(),
            StatusCode::UNAUTHORIZED,
            "GET {path} should 401 anonymously (got {})",
            res.status_code()
        );
    }
}

#[tokio::test]
async fn parent_post_likes_is_forbidden() {
    let (app, _auth) = boot_with_parent_and_child(AccountType::Parent).await;
    // No body needed — the gate fires before the handler reads it.
    let res = app.server.post("/api/likes/dQw4w9WgXcQ").await;
    assert_eq!(res.status_code(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn parent_post_timer_is_forbidden() {
    let (app, _auth) = boot_with_parent_and_child(AccountType::Parent).await;
    let res = app
        .server
        .post("/api/timer")
        .json(&json!({ "type": "minutes", "minutes": 30 }))
        .await;
    assert_eq!(res.status_code(), StatusCode::FORBIDDEN);
}
