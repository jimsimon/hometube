//! Extended allowlist tests — covers list, delete, and error cases for
//! channels and videos allowlist endpoints.
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
async fn add_video_with_no_metadata_and_no_sidecar_fails() {
    // When the discovery sidecar is unreachable (tests run without it)
    // and the request body carries only `video_id`, there is nothing
    // we can use as `video_title`. Writing such a row would make the
    // video invisible to the child-side search, so the handler must
    // refuse the request.
    let (app, _auth) = boot_with_parent_and_child(AccountType::Parent).await;
    let child_id = app.child_id.unwrap();

    let res = app
        .server
        .post(&format!("/api/children/{child_id}/allowlist/videos"))
        .json(&json!({ "video_id": "dQw4w9WgXcQ" }))
        .await;
    assert_eq!(res.status_code(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn add_video_uses_body_metadata_when_sidecar_unavailable() {
    // The parent UI already has title/channel_title/thumbnail_url from
    // the YouTube search response and passes them through. Even when
    // the discovery sidecar is down (as in this test environment), the
    // allowlist row should be persisted with a non-empty `video_title`
    // so that `/api/search` can find it later. This is the regression
    // we're fixing — previously the handler hard-failed without
    // sidecar data and the row was either rejected or saved with
    // `video_title = ""`.
    let (app, _auth) = boot_with_parent_and_child(AccountType::Parent).await;
    let child_id = app.child_id.unwrap();

    let res = app
        .server
        .post(&format!("/api/children/{child_id}/allowlist/videos"))
        .json(&json!({
            "video_id": "dQw4w9WgXcQ",
            "title": "Never Gonna Give You Up",
            "channel_title": "Rick Astley",
            "thumbnail_url": "https://img.example/rick.jpg",
        }))
        .await;
    assert_eq!(res.status_code(), StatusCode::OK);
    let body: serde_json::Value = res.json();
    assert_eq!(body["video_id"], "dQw4w9WgXcQ");
    assert_eq!(body["video_title"], "Never Gonna Give You Up");
    assert_eq!(body["channel_title"], "Rick Astley");
    assert_eq!(body["video_thumbnail_url"], "https://img.example/rick.jpg");

    // Title is persisted on disk — the search SQL filters on
    // `video_title LIKE …`, so this is the field that matters.
    let (db_title, db_channel): (String, Option<String>) = sqlx::query_as(
        "SELECT video_title, channel_title FROM allowlisted_videos \
         WHERE child_account_id = ? AND video_id = ?",
    )
    .bind(child_id)
    .bind("dQw4w9WgXcQ")
    .fetch_one(&app.pool)
    .await
    .unwrap();
    assert_eq!(db_title, "Never Gonna Give You Up");
    assert_eq!(db_channel.as_deref(), Some("Rick Astley"));
}

#[tokio::test]
async fn add_video_accepts_youtube_url_and_persists_body_title() {
    // The handler's `parse_video_id` extracts the bare ID from common
    // YouTube URL shapes. Combined with body metadata, a parent
    // pasting a full URL into the UI should still produce a
    // searchable row even without the sidecar.
    let (app, _auth) = boot_with_parent_and_child(AccountType::Parent).await;
    let child_id = app.child_id.unwrap();

    let res = app
        .server
        .post(&format!("/api/children/{child_id}/allowlist/videos"))
        .json(&json!({
            "video_id": "https://www.youtube.com/watch?v=abcDEF12345",
            "title": "Cool Video",
        }))
        .await;
    assert_eq!(res.status_code(), StatusCode::OK);
    let body: serde_json::Value = res.json();
    assert_eq!(body["video_id"], "abcDEF12345");
    assert_eq!(body["video_title"], "Cool Video");
}

#[tokio::test]
async fn add_video_rejects_body_with_blank_title() {
    // A whitespace-only title is just as useless as no title — the
    // LIKE search can never match it. Treat it as "missing".
    let (app, _auth) = boot_with_parent_and_child(AccountType::Parent).await;
    let child_id = app.child_id.unwrap();

    let res = app
        .server
        .post(&format!("/api/children/{child_id}/allowlist/videos"))
        .json(&json!({ "video_id": "dQw4w9WgXcQ", "title": "   " }))
        .await;
    assert_eq!(res.status_code(), StatusCode::BAD_REQUEST);
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
async fn allowlist_rejects_parent_target_for_videos() {
    let (app, _auth) = boot_with_parent_and_child(AccountType::Parent).await;
    let parent_id = app.parent_id.unwrap();

    let res = app
        .server
        .get(&format!("/api/children/{parent_id}/allowlist/videos"))
        .await;
    assert_eq!(res.status_code(), StatusCode::BAD_REQUEST);
}
