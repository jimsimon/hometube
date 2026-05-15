//! Extended allowlist tests — covers list, delete, and error cases for
//! channels, playlists, and videos allowlist endpoints.
//!
//! The add (POST) endpoints call the YouTube API for metadata resolution,
//! which fails with fake keys. The list/delete operations work entirely
//! against the database and can be driven by seeding rows directly.

mod common;

use axum::http::StatusCode;
use common::boot_with_parent_and_child;
use hometube::models::account::AccountType;
use serde_json::json;

// ===========================================================================
// Channels
// ===========================================================================

#[tokio::test]
async fn list_channels_initially_empty() {
    let (app, _auth) = boot_with_parent_and_child(AccountType::Parent).await;
    let child_id = app.child_id.unwrap();

    let res = app
        .server
        .get(&format!("/api/children/{child_id}/allowlist/channels"))
        .await;
    assert_eq!(res.status_code(), StatusCode::OK);
    let body: serde_json::Value = res.json();
    assert!(body.as_array().unwrap().is_empty());
}

#[tokio::test]
async fn list_channels_returns_seeded_rows() {
    let (app, _auth) = boot_with_parent_and_child(AccountType::Parent).await;
    let child_id = app.child_id.unwrap();
    let parent_id = app.parent_id.unwrap();

    sqlx::query(
        "INSERT INTO allowlisted_channels (child_account_id, channel_id, channel_title, added_by) \
         VALUES (?, 'UCch1', 'Channel One', ?)",
    )
    .bind(child_id)
    .bind(parent_id)
    .execute(&app.pool)
    .await
    .unwrap();

    let res = app
        .server
        .get(&format!("/api/children/{child_id}/allowlist/channels"))
        .await;
    assert_eq!(res.status_code(), StatusCode::OK);
    let body: serde_json::Value = res.json();
    let arr = body.as_array().unwrap();
    assert_eq!(arr.len(), 1);
    assert_eq!(arr[0]["channel_id"], "UCch1");
    assert_eq!(arr[0]["channel_title"], "Channel One");
}

#[tokio::test]
async fn delete_channel_from_allowlist() {
    let (app, _auth) = boot_with_parent_and_child(AccountType::Parent).await;
    let child_id = app.child_id.unwrap();
    let parent_id = app.parent_id.unwrap();

    sqlx::query(
        "INSERT INTO allowlisted_channels (child_account_id, channel_id, channel_title, added_by) \
         VALUES (?, 'UCdel', 'To Delete', ?)",
    )
    .bind(child_id)
    .bind(parent_id)
    .execute(&app.pool)
    .await
    .unwrap();

    let res = app
        .server
        .delete(&format!(
            "/api/children/{child_id}/allowlist/channels/UCdel"
        ))
        .await;
    assert_eq!(res.status_code(), StatusCode::NO_CONTENT);

    // Verify it's gone.
    let count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM allowlisted_channels WHERE child_account_id = ? AND channel_id = 'UCdel'",
    )
    .bind(child_id)
    .fetch_one(&app.pool)
    .await
    .unwrap();
    assert_eq!(count, 0);
}

#[tokio::test]
async fn add_channel_with_fake_key_fails() {
    let (app, _auth) = boot_with_parent_and_child(AccountType::Parent).await;
    let child_id = app.child_id.unwrap();

    let res = app
        .server
        .post(&format!("/api/children/{child_id}/allowlist/channels"))
        .json(&json!({ "channel_id": "UCfake" }))
        .await;
    let status = res.status_code().as_u16();
    assert!(status >= 400);
}

// ===========================================================================
// Playlists
// ===========================================================================

#[tokio::test]
async fn list_playlists_initially_empty() {
    let (app, _auth) = boot_with_parent_and_child(AccountType::Parent).await;
    let child_id = app.child_id.unwrap();

    let res = app
        .server
        .get(&format!("/api/children/{child_id}/allowlist/playlists"))
        .await;
    assert_eq!(res.status_code(), StatusCode::OK);
    let body: serde_json::Value = res.json();
    assert!(body.as_array().unwrap().is_empty());
}

#[tokio::test]
async fn list_playlists_returns_seeded_rows() {
    let (app, _auth) = boot_with_parent_and_child(AccountType::Parent).await;
    let child_id = app.child_id.unwrap();
    let parent_id = app.parent_id.unwrap();

    sqlx::query(
        "INSERT INTO allowlisted_playlists (child_account_id, playlist_id, playlist_title, added_by) \
         VALUES (?, 'PL123', 'My Playlist', ?)",
    )
    .bind(child_id)
    .bind(parent_id)
    .execute(&app.pool)
    .await
    .unwrap();

    let res = app
        .server
        .get(&format!("/api/children/{child_id}/allowlist/playlists"))
        .await;
    let body: serde_json::Value = res.json();
    let arr = body.as_array().unwrap();
    assert_eq!(arr.len(), 1);
    assert_eq!(arr[0]["playlist_id"], "PL123");
}

#[tokio::test]
async fn delete_playlist_from_allowlist() {
    let (app, _auth) = boot_with_parent_and_child(AccountType::Parent).await;
    let child_id = app.child_id.unwrap();
    let parent_id = app.parent_id.unwrap();

    sqlx::query(
        "INSERT INTO allowlisted_playlists (child_account_id, playlist_id, playlist_title, added_by) \
         VALUES (?, 'PLdel', 'To Delete', ?)",
    )
    .bind(child_id)
    .bind(parent_id)
    .execute(&app.pool)
    .await
    .unwrap();

    let res = app
        .server
        .delete(&format!(
            "/api/children/{child_id}/allowlist/playlists/PLdel"
        ))
        .await;
    assert_eq!(res.status_code(), StatusCode::NO_CONTENT);
}

// ===========================================================================
// Videos
// ===========================================================================

#[tokio::test]
async fn list_videos_initially_empty() {
    let (app, _auth) = boot_with_parent_and_child(AccountType::Parent).await;
    let child_id = app.child_id.unwrap();

    let res = app
        .server
        .get(&format!("/api/children/{child_id}/allowlist/videos"))
        .await;
    assert_eq!(res.status_code(), StatusCode::OK);
    let body: serde_json::Value = res.json();
    assert!(body.as_array().unwrap().is_empty());
}

#[tokio::test]
async fn list_videos_returns_seeded_rows() {
    let (app, _auth) = boot_with_parent_and_child(AccountType::Parent).await;
    let child_id = app.child_id.unwrap();
    let parent_id = app.parent_id.unwrap();

    sqlx::query(
        "INSERT INTO allowlisted_videos (child_account_id, video_id, video_title, channel_title, added_by) \
         VALUES (?, 'vid-A', 'Video A', 'Ch', ?)",
    )
    .bind(child_id)
    .bind(parent_id)
    .execute(&app.pool)
    .await
    .unwrap();

    let res = app
        .server
        .get(&format!("/api/children/{child_id}/allowlist/videos"))
        .await;
    let body: serde_json::Value = res.json();
    let arr = body.as_array().unwrap();
    assert_eq!(arr.len(), 1);
    assert_eq!(arr[0]["video_id"], "vid-A");
    assert_eq!(arr[0]["channel_title"], "Ch");
}

#[tokio::test]
async fn delete_video_from_allowlist() {
    let (app, _auth) = boot_with_parent_and_child(AccountType::Parent).await;
    let child_id = app.child_id.unwrap();
    let parent_id = app.parent_id.unwrap();

    sqlx::query(
        "INSERT INTO allowlisted_videos (child_account_id, video_id, video_title, added_by) \
         VALUES (?, 'vid-del', 'Del', ?)",
    )
    .bind(child_id)
    .bind(parent_id)
    .execute(&app.pool)
    .await
    .unwrap();

    let res = app
        .server
        .delete(&format!(
            "/api/children/{child_id}/allowlist/videos/vid-del"
        ))
        .await;
    assert_eq!(res.status_code(), StatusCode::NO_CONTENT);
}

#[tokio::test]
async fn add_video_with_fake_key_fails() {
    let (app, _auth) = boot_with_parent_and_child(AccountType::Parent).await;
    let child_id = app.child_id.unwrap();

    let res = app
        .server
        .post(&format!("/api/children/{child_id}/allowlist/videos"))
        .json(&json!({ "video_id": "dQw4w9WgXcQ" }))
        .await;
    let status = res.status_code().as_u16();
    assert!(status >= 400);
}

// ===========================================================================
// Error cases
// ===========================================================================

#[tokio::test]
async fn allowlist_rejects_parent_target_for_channels() {
    let (app, _auth) = boot_with_parent_and_child(AccountType::Parent).await;
    let parent_id = app.parent_id.unwrap();

    let res = app
        .server
        .get(&format!("/api/children/{parent_id}/allowlist/channels"))
        .await;
    assert_eq!(res.status_code(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn allowlist_rejects_parent_target_for_playlists() {
    let (app, _auth) = boot_with_parent_and_child(AccountType::Parent).await;
    let parent_id = app.parent_id.unwrap();

    let res = app
        .server
        .get(&format!("/api/children/{parent_id}/allowlist/playlists"))
        .await;
    assert_eq!(res.status_code(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn allowlist_rejects_parent_target_for_videos() {
    let (app, _auth) = boot_with_parent_and_child(AccountType::Parent).await;
    let parent_id = app.parent_id.unwrap();

    let res = app
        .server
        .get(&format!("/api/children/{parent_id}/allowlist/videos"))
        .await;
    assert_eq!(res.status_code(), StatusCode::BAD_REQUEST);
}
