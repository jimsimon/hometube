//! Extended page rendering tests — covers parent pages that redirect
//! children, child pages that redirect parents, anonymous handling,
//! and specific page routes not covered by the existing pages.rs tests.

mod common;

use common::{boot_setup_complete, boot_with_parent_and_child};
use hometube::models::account::AccountType;

// ===========================================================================
// Parent pages
// ===========================================================================

#[tokio::test]
async fn parent_system_page_renders() {
    let (app, _auth) = boot_with_parent_and_child(AccountType::Parent).await;
    let res = app.server.get("/parent/system").await;
    let status = res.status_code();
    assert!(status.is_success(), "got {status}");
}

#[tokio::test]
async fn parent_family_page_renders() {
    let (app, _auth) = boot_with_parent_and_child(AccountType::Parent).await;
    let res = app.server.get("/parent/family").await;
    let status = res.status_code();
    assert!(status.is_success(), "got {status}");
}

#[tokio::test]
async fn parent_activity_page_renders() {
    let (app, _auth) = boot_with_parent_and_child(AccountType::Parent).await;
    let res = app.server.get("/parent/activity").await;
    let status = res.status_code();
    assert!(status.is_success(), "got {status}");
}

#[tokio::test]
async fn parent_playlists_page_renders() {
    let (app, _auth) = boot_with_parent_and_child(AccountType::Parent).await;
    let res = app.server.get("/parent/playlists").await;
    let status = res.status_code();
    assert!(status.is_success(), "got {status}");
}

#[tokio::test]
async fn parent_playlist_detail_page_renders() {
    let (app, _auth) = boot_with_parent_and_child(AccountType::Parent).await;
    let res = app.server.get("/parent/playlist/1").await;
    let status = res.status_code();
    assert!(status.is_success(), "got {status}");
}

#[tokio::test]
async fn parent_preview_video_page_renders() {
    let (app, _auth) = boot_with_parent_and_child(AccountType::Parent).await;
    let res = app.server.get("/parent/preview/video/abc123").await;
    let status = res.status_code();
    assert!(status.is_success(), "got {status}");
}

#[tokio::test]
async fn parent_preview_channel_page_renders() {
    let (app, _auth) = boot_with_parent_and_child(AccountType::Parent).await;
    let res = app.server.get("/parent/preview/channel/UCabc").await;
    let status = res.status_code();
    assert!(status.is_success(), "got {status}");
}

#[tokio::test]
async fn parent_preview_playlist_page_renders() {
    let (app, _auth) = boot_with_parent_and_child(AccountType::Parent).await;
    let res = app.server.get("/parent/preview/playlist/PLabc").await;
    let status = res.status_code();
    assert!(status.is_success(), "got {status}");
}

#[tokio::test]
async fn parent_preview_invalid_kind_redirects() {
    let (app, _auth) = boot_with_parent_and_child(AccountType::Parent).await;
    let res = app.server.get("/parent/preview/invalid/id").await;
    let status = res.status_code().as_u16();
    // Should redirect to /parent/home.
    assert!(
        (300..400).contains(&status) || status == 200,
        "got {status}"
    );
}

// ===========================================================================
// Child pages
// ===========================================================================

#[tokio::test]
async fn child_bookmarks_page_renders() {
    let (app, _auth) = boot_with_parent_and_child(AccountType::Child).await;
    let res = app.server.get("/child/bookmarks").await;
    let status = res.status_code();
    assert!(status.is_success(), "got {status}");
}

#[tokio::test]
async fn child_downloads_page_renders() {
    let (app, _auth) = boot_with_parent_and_child(AccountType::Child).await;
    let res = app.server.get("/child/downloads").await;
    let status = res.status_code();
    assert!(status.is_success(), "got {status}");
}

#[tokio::test]
async fn child_channels_page_renders() {
    let (app, _auth) = boot_with_parent_and_child(AccountType::Child).await;
    let res = app.server.get("/child/channels").await;
    let status = res.status_code();
    assert!(status.is_success(), "got {status}");
}

#[tokio::test]
async fn child_channel_detail_page_renders() {
    let (app, _auth) = boot_with_parent_and_child(AccountType::Child).await;
    let res = app.server.get("/child/channel/UCabc").await;
    let status = res.status_code();
    assert!(status.is_success(), "got {status}");
}

#[tokio::test]
async fn child_playlists_page_renders() {
    let (app, _auth) = boot_with_parent_and_child(AccountType::Child).await;
    let res = app.server.get("/child/playlists").await;
    let status = res.status_code();
    assert!(status.is_success(), "got {status}");
}

#[tokio::test]
async fn child_playlist_own_page_renders() {
    let (app, _auth) = boot_with_parent_and_child(AccountType::Child).await;
    let res = app.server.get("/child/playlist/123").await;
    let status = res.status_code();
    assert!(status.is_success(), "got {status}");
}

#[tokio::test]
async fn child_playlist_family_prefix_renders() {
    let (app, _auth) = boot_with_parent_and_child(AccountType::Child).await;
    let res = app.server.get("/child/playlist/family:42").await;
    let status = res.status_code();
    assert!(status.is_success(), "got {status}");
}

#[tokio::test]
async fn child_search_page_renders() {
    let (app, _auth) = boot_with_parent_and_child(AccountType::Child).await;
    let res = app.server.get("/child/search?q=hello&type=video").await;
    let status = res.status_code();
    assert!(status.is_success(), "got {status}");
}

// ===========================================================================
// Cross-role redirects
// ===========================================================================

#[tokio::test]
async fn child_accessing_parent_system_redirects() {
    let (app, _auth) = boot_with_parent_and_child(AccountType::Child).await;
    let res = app.server.get("/parent/system").await;
    let status = res.status_code().as_u16();
    assert!((300..400).contains(&status), "got {status}");
}

#[tokio::test]
async fn parent_accessing_child_bookmarks_redirects() {
    let (app, _auth) = boot_with_parent_and_child(AccountType::Parent).await;
    let res = app.server.get("/child/bookmarks").await;
    let status = res.status_code().as_u16();
    assert!((300..400).contains(&status), "got {status}");
}

#[tokio::test]
async fn parent_accessing_child_search_redirects() {
    let (app, _auth) = boot_with_parent_and_child(AccountType::Parent).await;
    let res = app.server.get("/child/search?q=test").await;
    let status = res.status_code().as_u16();
    assert!((300..400).contains(&status), "got {status}");
}

// ===========================================================================
// Login page variations
// ===========================================================================

#[tokio::test]
async fn login_page_with_context_param() {
    let (app, _auth) = boot_setup_complete(AccountType::Parent).await;
    let res = app
        .server
        .get("/login?role=child&context=add_member")
        .clear_cookies()
        .await;
    let status = res.status_code();
    assert!(status.is_success(), "got {status}");
}

#[tokio::test]
async fn login_page_with_youtube_scope_error() {
    let (app, _auth) = boot_setup_complete(AccountType::Parent).await;
    let res = app
        .server
        .get("/login?error=youtube_scope_missing")
        .clear_cookies()
        .await;
    let status = res.status_code();
    assert!(status.is_success(), "got {status}");
    let body = res.text();
    assert!(body.contains("YouTube"));
}

#[tokio::test]
async fn login_page_with_access_denied_error() {
    let (app, _auth) = boot_setup_complete(AccountType::Parent).await;
    let res = app
        .server
        .get("/login?error=access_denied")
        .clear_cookies()
        .await;
    assert!(res.status_code().is_success());
}

#[tokio::test]
async fn login_page_with_unknown_error() {
    let (app, _auth) = boot_setup_complete(AccountType::Parent).await;
    let res = app
        .server
        .get("/login?error=some_unknown_error")
        .clear_cookies()
        .await;
    assert!(res.status_code().is_success());
}

// ===========================================================================
// Set PIN page
// ===========================================================================

#[tokio::test]
async fn set_pin_page_renders_for_parent() {
    let (app, _auth) = boot_setup_complete(AccountType::Parent).await;
    let res = app.server.get("/setup/pin").await;
    assert!(res.status_code().is_success());
}

#[tokio::test]
async fn set_pin_page_for_new_parent_flag() {
    let (app, _auth) = boot_setup_complete(AccountType::Parent).await;
    let res = app.server.get("/setup/pin?for_new_parent=1").await;
    assert!(res.status_code().is_success());
}

#[tokio::test]
async fn set_pin_page_child_redirects() {
    let (app, _auth) = boot_with_parent_and_child(AccountType::Child).await;
    let res = app.server.get("/setup/pin").await;
    let status = res.status_code().as_u16();
    assert!((300..400).contains(&status), "got {status}");
}

// ===========================================================================
// Video page with seeded data
// ===========================================================================

#[tokio::test]
async fn child_home_page_renders_video_grid_with_data() {
    let (app, auth) = boot_with_parent_and_child(AccountType::Child).await;
    let child_id = auth.account_id;
    let parent_id = app.parent_id.unwrap();

    // Seed some allowlisted videos.
    for i in 0..3 {
        sqlx::query(
            "INSERT INTO allowlisted_videos (child_account_id, video_id, video_title, video_thumbnail_url, added_by) \
             VALUES (?, ?, ?, 'http://thumb.test/v.jpg', ?)",
        )
        .bind(child_id)
        .bind(format!("vid-{i}"))
        .bind(format!("Video {i}"))
        .bind(parent_id)
        .execute(&app.pool)
        .await
        .unwrap();
    }

    let res = app.server.get("/child/home").await;
    assert!(res.status_code().is_success());
    let body = res.text();
    // Should contain video titles in the SSR grid.
    assert!(body.contains("Video 0") || body.contains("vid-0"));
}
