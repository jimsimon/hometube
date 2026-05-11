//! Allowlist read/delete coverage.
//!
//! `POST /api/children/:id/allowlist/{kind}` calls
//! [`hometube::services::youtube::YoutubeClient`] which goes off-network
//! to the live YouTube Data API — we never exercise the create path
//! through the API in these tests. Instead we insert allowlist rows
//! directly into the database (matching what a successful
//! YouTube-resolved POST would write) and then assert that GET + DELETE
//! behave correctly.
//!
//! This still meaningfully covers the routes' SQL + serialization
//! paths and the `require_child_id` validation helper.

mod common;

use axum::http::StatusCode;
use common::boot_with_parent_and_child;
use hometube::models::account::AccountType;

#[tokio::test]
async fn videos_round_trip_via_db_seed() {
    let (app, _auth) = boot_with_parent_and_child(AccountType::Parent).await;
    let child_id = app.child_id.unwrap();
    let parent_id = app.parent_id.unwrap();

    sqlx::query(
        "INSERT INTO allowlisted_videos \
            (child_account_id, video_id, video_title, video_thumbnail_url, channel_title, added_by) \
         VALUES (?, 'vid-1', 'Hello', 'http://thumb', 'Some Channel', ?)",
    )
    .bind(child_id)
    .bind(parent_id)
    .execute(&app.pool)
    .await
    .expect("seed video");

    // Pre-populate `video_metadata_cache` so any handler that does a
    // best-effort lookup hits the cache rather than yt-dlp.
    sqlx::query(
        "INSERT INTO video_metadata_cache (video_id, metadata_json, expires_at) \
         VALUES ('vid-1', '{\"id\":\"vid-1\",\"channel_id\":\"chan-1\"}', unixepoch() + 3600)",
    )
    .execute(&app.pool)
    .await
    .expect("seed metadata cache");

    let res = app
        .server
        .get(&format!("/api/children/{child_id}/allowlist/videos"))
        .await;
    assert_eq!(res.status_code(), StatusCode::OK);
    let body: serde_json::Value = res.json();
    let arr = body.as_array().unwrap();
    assert_eq!(arr.len(), 1);
    assert_eq!(arr[0]["video_id"], "vid-1");
    assert_eq!(arr[0]["video_title"], "Hello");

    // Delete and confirm the list is empty.
    let res = app
        .server
        .delete(&format!("/api/children/{child_id}/allowlist/videos/vid-1"))
        .await;
    assert_eq!(res.status_code(), StatusCode::NO_CONTENT);

    let res = app
        .server
        .get(&format!("/api/children/{child_id}/allowlist/videos"))
        .await;
    let body: serde_json::Value = res.json();
    assert!(body.as_array().unwrap().is_empty());
}

#[tokio::test]
async fn channels_round_trip_via_db_seed() {
    let (app, _auth) = boot_with_parent_and_child(AccountType::Parent).await;
    let child_id = app.child_id.unwrap();
    let parent_id = app.parent_id.unwrap();

    sqlx::query(
        "INSERT INTO allowlisted_channels \
            (child_account_id, channel_id, channel_title, channel_thumbnail_url, added_by) \
         VALUES (?, 'chan-1', 'Cool Channel', NULL, ?)",
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
    let arr: serde_json::Value = res.json();
    assert_eq!(arr[0]["channel_id"], "chan-1");

    let res = app
        .server
        .delete(&format!(
            "/api/children/{child_id}/allowlist/channels/chan-1"
        ))
        .await;
    assert_eq!(res.status_code(), StatusCode::NO_CONTENT);
}

#[tokio::test]
async fn playlists_round_trip_via_db_seed() {
    let (app, _auth) = boot_with_parent_and_child(AccountType::Parent).await;
    let child_id = app.child_id.unwrap();
    let parent_id = app.parent_id.unwrap();

    sqlx::query(
        "INSERT INTO allowlisted_playlists \
            (child_account_id, playlist_id, playlist_title, playlist_thumbnail_url, added_by) \
         VALUES (?, 'pl-1', 'My PL', NULL, ?)",
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
    assert_eq!(res.status_code(), StatusCode::OK);

    let res = app
        .server
        .delete(&format!(
            "/api/children/{child_id}/allowlist/playlists/pl-1"
        ))
        .await;
    assert_eq!(res.status_code(), StatusCode::NO_CONTENT);
}

#[tokio::test]
async fn allowlist_rejects_non_child_target() {
    let (app, _auth) = boot_with_parent_and_child(AccountType::Parent).await;
    let parent_id = app.parent_id.unwrap();
    // Pointing at a parent ID returns 400 from `require_child_id`.
    let res = app
        .server
        .get(&format!("/api/children/{parent_id}/allowlist/videos"))
        .await;
    assert_eq!(res.status_code(), StatusCode::BAD_REQUEST);
}
