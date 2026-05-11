//! HTML-page coverage.
//!
//! `routes/pages.rs` is mostly straightforward askama rendering, so the
//! best signal we can get cheaply is "the templates compile, hydrate,
//! and produce a 2xx (or expected redirect) for each role." We sweep
//! every page route as parent, child, and anonymous and assert the
//! response's status maps to the documented role gate.

mod common;

use axum::http::StatusCode;
use common::{boot, boot_with_parent_and_child};
use hometube::middleware::auth::SESSION_COOKIE;
use hometube::models::account::AccountType;
use tower_cookies::cookie::Cookie;

const PARENT_ONLY_PAGES: &[&str] = &[
    "/parent/home",
    "/parent/family",
    "/parent/system",
    "/parent/activity",
    "/parent/playlists",
];

const CHILD_ONLY_PAGES: &[&str] = &[
    "/child/home",
    "/child/channels",
    "/child/playlists",
    "/child/bookmarks",
    "/child/downloads",
    "/child/search",
];

#[tokio::test]
async fn parent_pages_render_for_a_parent() {
    let (app, _auth) = boot_with_parent_and_child(AccountType::Parent).await;
    for path in PARENT_ONLY_PAGES {
        let res = app.server.get(path).await;
        let s = res.status_code();
        assert!(
            s.is_success() || s.is_redirection(),
            "{path}: expected 2xx/3xx, got {s}"
        );
    }
}

#[tokio::test]
async fn child_pages_render_for_a_child() {
    let (app, _auth) = boot_with_parent_and_child(AccountType::Child).await;
    for path in CHILD_ONLY_PAGES {
        let res = app.server.get(path).await;
        let s = res.status_code();
        assert!(
            s.is_success() || s.is_redirection(),
            "{path}: expected 2xx/3xx, got {s}"
        );
    }
}

#[tokio::test]
async fn parent_pages_redirect_a_child_user() {
    let (app, _auth) = boot_with_parent_and_child(AccountType::Child).await;
    for path in PARENT_ONLY_PAGES {
        let res = app.server.get(path).await;
        let s = res.status_code();
        // Either 303 redirect or 4xx — never a 2xx page.
        assert!(
            !s.is_success(),
            "{path}: a child must not see the parent page (got {s})"
        );
    }
}

#[tokio::test]
async fn child_pages_redirect_a_parent_user() {
    let (app, _auth) = boot_with_parent_and_child(AccountType::Parent).await;
    for path in CHILD_ONLY_PAGES {
        let res = app.server.get(path).await;
        let s = res.status_code();
        assert!(
            !s.is_success(),
            "{path}: a parent must not see the child page (got {s})"
        );
    }
}

#[tokio::test]
async fn anonymous_pages_redirect_when_setup_incomplete() {
    let app = boot().await;
    // setup_redirect rewrites every page to `/setup`.
    for path in &["/parent/home", "/child/home", "/profiles"] {
        let res = app.server.get(path).await;
        let s = res.status_code();
        assert!(s.is_redirection(), "{path}: expected redirect, got {s}");
    }
}

#[tokio::test]
async fn setup_wizard_renders_for_anonymous_users() {
    let app = boot().await;
    let res = app.server.get("/setup").await;
    assert_eq!(res.status_code(), StatusCode::OK);
    assert!(res.text().contains("HomeTube") || res.text().contains("setup"));
}

#[tokio::test]
async fn root_redirects_anonymous_after_setup_to_profile_picker() {
    let app = boot().await;
    common::seed_credentials(&app.pool).await;
    common::insert_account(&app.pool, "g1", "p@e.t", "P", AccountType::Parent).await;
    hometube::services::setup::set_config_value(
        &app.pool,
        hometube::services::setup::KEY_SETUP_COMPLETE,
        "true",
    )
    .await
    .unwrap();
    let res = app.server.get("/").await;
    assert!(res.status_code().is_redirection());
    assert_eq!(res.headers().get("location").unwrap(), "/profiles");
}

#[tokio::test]
async fn root_routes_a_parent_to_parent_home() {
    let (app, _auth) = boot_with_parent_and_child(AccountType::Parent).await;
    let res = app.server.get("/").await;
    assert!(res.status_code().is_redirection());
    assert_eq!(res.headers().get("location").unwrap(), "/parent/home");
}

#[tokio::test]
async fn root_routes_a_child_to_child_home() {
    let (app, _auth) = boot_with_parent_and_child(AccountType::Child).await;
    let res = app.server.get("/").await;
    assert!(res.status_code().is_redirection());
    assert_eq!(res.headers().get("location").unwrap(), "/child/home");
}

#[tokio::test]
async fn profiles_page_renders() {
    let (app, _auth) = boot_with_parent_and_child(AccountType::Parent).await;
    // Strip session cookie so we hit the picker as anonymous.
    let bad = Cookie::new(SESSION_COOKIE, "junk");
    let res = app
        .server
        .get("/profiles")
        .clear_cookies()
        .add_cookie(bad)
        .await;
    assert!(res.status_code().is_success() || res.status_code().is_redirection());
}
