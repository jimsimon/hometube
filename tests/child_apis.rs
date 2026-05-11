//! Broad coverage of child-only routes via direct DB seeding.
//!
//! Many child routes upsert YouTube state at write time
//! (subscriptions, likes, playlists library-add) but accept simple
//! reads or deletes against pre-existing rows. We seed the DB
//! directly to drive the route handlers through their happy paths
//! without going off-network.

mod common;

use axum::http::StatusCode;
use common::boot_with_parent_and_child;
use hometube::models::account::AccountType;
use serde_json::json;

#[tokio::test]
async fn feed_continue_watching_returns_seeded_history() {
    let (app, auth) = boot_with_parent_and_child(AccountType::Child).await;
    let child_id = auth.account_id;

    // Need an allowlist entry, since `continue_watching` filters by
    // `can_child_view`.
    sqlx::query(
        "INSERT INTO allowlisted_videos (child_account_id, video_id, video_title, added_by) \
         VALUES (?, 'vid-1', 'Hello', ?)",
    )
    .bind(child_id)
    .bind(app.parent_id.unwrap())
    .execute(&app.pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO watch_history (child_account_id, video_id, video_title, progress_seconds, last_watched_at) \
         VALUES (?, 'vid-1', 'Hello', 60, unixepoch())",
    )
    .bind(child_id)
    .execute(&app.pool)
    .await
    .unwrap();

    let res = app.server.get("/api/feed/continue-watching").await;
    assert_eq!(res.status_code(), StatusCode::OK);
    let body: serde_json::Value = res.json();
    let arr = body.as_array().unwrap();
    assert!(
        !arr.is_empty(),
        "expected continue-watching to surface seeded row"
    );
}

#[tokio::test]
async fn feed_continue_watching_filters_blocked_videos() {
    let (app, auth) = boot_with_parent_and_child(AccountType::Child).await;
    let child_id = auth.account_id;
    let parent_id = app.parent_id.unwrap();

    // Allowlist + watch-history a video, then block it.
    sqlx::query(
        "INSERT INTO allowlisted_videos (child_account_id, video_id, video_title, added_by) \
         VALUES (?, 'vid-1', 'T', ?)",
    )
    .bind(child_id)
    .bind(parent_id)
    .execute(&app.pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO watch_history (child_account_id, video_id, video_title, last_watched_at) \
         VALUES (?, 'vid-1', 'T', unixepoch())",
    )
    .bind(child_id)
    .execute(&app.pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO blocked_videos (child_account_id, video_id, blocked_by) \
         VALUES (?, 'vid-1', ?)",
    )
    .bind(child_id)
    .bind(parent_id)
    .execute(&app.pool)
    .await
    .unwrap();

    let res = app.server.get("/api/feed/continue-watching").await;
    let body: serde_json::Value = res.json();
    assert!(
        body.as_array().unwrap().is_empty(),
        "blocked video must be filtered out of continue-watching"
    );
}

#[tokio::test]
async fn feed_new_videos_returns_empty_with_no_allowlist() {
    let (app, _auth) = boot_with_parent_and_child(AccountType::Child).await;
    let res = app.server.get("/api/feed/new-videos").await;
    assert_eq!(res.status_code(), StatusCode::OK);
    let body: serde_json::Value = res.json();
    assert!(body.is_array());
}

#[tokio::test]
async fn feed_new_videos_returns_empty_when_yt_key_missing() {
    let (app, _auth) = boot_with_parent_and_child(AccountType::Child).await;
    // Wipe the youtube_api_key so YoutubeClient::from_db fails. The
    // handler returns an empty array in that case.
    sqlx::query("DELETE FROM app_config WHERE key = 'youtube_api_key'")
        .execute(&app.pool)
        .await
        .unwrap();
    let res = app.server.get("/api/feed/new-videos").await;
    assert_eq!(res.status_code(), StatusCode::OK);
    let body: serde_json::Value = res.json();
    assert!(body.as_array().unwrap().is_empty());
}

#[tokio::test]
async fn subscriptions_list_returns_seeded_rows() {
    let (app, auth) = boot_with_parent_and_child(AccountType::Child).await;
    let child_id = auth.account_id;
    sqlx::query(
        "INSERT INTO child_subscriptions \
            (child_account_id, channel_id, channel_title) \
         VALUES (?, 'chan-1', 'Some Channel')",
    )
    .bind(child_id)
    .execute(&app.pool)
    .await
    .unwrap();

    let res = app.server.get("/api/subscriptions").await;
    assert_eq!(res.status_code(), StatusCode::OK);
    let body: serde_json::Value = res.json();
    assert_eq!(body[0]["channel_id"], "chan-1");
}

#[tokio::test]
async fn subscriptions_unsubscribe_marks_pending_delete() {
    let (app, auth) = boot_with_parent_and_child(AccountType::Child).await;
    let child_id = auth.account_id;
    sqlx::query(
        "INSERT INTO child_subscriptions \
            (child_account_id, channel_id, channel_title) \
         VALUES (?, 'chan-1', 'Some Channel')",
    )
    .bind(child_id)
    .execute(&app.pool)
    .await
    .unwrap();

    let res = app.server.delete("/api/subscriptions/chan-1").await;
    assert_eq!(res.status_code(), StatusCode::NO_CONTENT);

    let (deleted, status): (i64, String) = sqlx::query_as(
        "SELECT is_deleted, sync_status FROM child_subscriptions WHERE channel_id = ?",
    )
    .bind("chan-1")
    .fetch_one(&app.pool)
    .await
    .unwrap();
    assert_eq!(deleted, 1);
    assert_eq!(status, "pending_delete");
}

#[tokio::test]
async fn subscriptions_unsubscribe_404_when_not_found() {
    let (app, _auth) = boot_with_parent_and_child(AccountType::Child).await;
    let res = app
        .server
        .delete("/api/subscriptions/no-such-channel")
        .await;
    assert_eq!(res.status_code(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn unlike_seeded_row() {
    let (app, auth) = boot_with_parent_and_child(AccountType::Child).await;
    let child_id = auth.account_id;
    sqlx::query(
        "INSERT INTO video_likes (child_account_id, video_id, video_title) \
         VALUES (?, 'vid-1', 'Hello')",
    )
    .bind(child_id)
    .execute(&app.pool)
    .await
    .unwrap();

    let res = app.server.delete("/api/likes/vid-1").await;
    assert_eq!(res.status_code(), StatusCode::NO_CONTENT);
}

#[tokio::test]
async fn unlike_404_for_missing() {
    let (app, _auth) = boot_with_parent_and_child(AccountType::Child).await;
    let res = app.server.delete("/api/likes/no-such-vid").await;
    assert_eq!(res.status_code(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn likes_list_returns_seeded_rows() {
    let (app, auth) = boot_with_parent_and_child(AccountType::Child).await;
    let child_id = auth.account_id;
    sqlx::query(
        "INSERT INTO video_likes (child_account_id, video_id, video_title) \
         VALUES (?, 'vid-1', 'Hello')",
    )
    .bind(child_id)
    .execute(&app.pool)
    .await
    .unwrap();

    let res = app.server.get("/api/likes").await;
    assert_eq!(res.status_code(), StatusCode::OK);
    let body: serde_json::Value = res.json();
    assert_eq!(body[0]["video_id"], "vid-1");
}

#[tokio::test]
async fn playlists_create_and_list() {
    let (app, _auth) = boot_with_parent_and_child(AccountType::Child).await;
    let res = app
        .server
        .post("/api/playlists")
        .json(&json!({ "title": "My Mix", "description": "fun stuff" }))
        .await;
    assert!(res.status_code().is_success());

    let res = app.server.get("/api/playlists").await;
    assert_eq!(res.status_code(), StatusCode::OK);
    let body: serde_json::Value = res.json();
    assert_eq!(body[0]["title"], "My Mix");
}

#[tokio::test]
async fn playlists_create_with_empty_title_400() {
    let (app, _auth) = boot_with_parent_and_child(AccountType::Child).await;
    let res = app
        .server
        .post("/api/playlists")
        .json(&json!({ "title": "", "description": "" }))
        .await;
    assert_eq!(res.status_code(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn bookmarks_crud() {
    let (app, _auth) = boot_with_parent_and_child(AccountType::Child).await;

    let res = app
        .server
        .post("/api/bookmarks")
        .json(&json!({
            "video_id": "vid-1",
            "video_title": "Hello",
            "timestamp_seconds": 30,
            "label": "good part",
        }))
        .await;
    assert!(res.status_code().is_success());
    let body: serde_json::Value = res.json();
    let id = body["id"].as_i64().unwrap();

    // List all.
    let res = app.server.get("/api/bookmarks").await;
    assert!(res.status_code().is_success());
    let body: serde_json::Value = res.json();
    assert!(!body.as_array().unwrap().is_empty());

    // Update label.
    let res = app
        .server
        .put(&format!("/api/bookmarks/{id}"))
        .json(&json!({ "label": "great part" }))
        .await;
    assert!(res.status_code().is_success());

    // Delete.
    let res = app.server.delete(&format!("/api/bookmarks/{id}")).await;
    assert_eq!(res.status_code(), StatusCode::NO_CONTENT);
}

#[tokio::test]
async fn bookmarks_for_video_returns_only_matches() {
    let (app, _auth) = boot_with_parent_and_child(AccountType::Child).await;
    app.server
        .post("/api/bookmarks")
        .json(&json!({
            "video_id": "vid-a",
            "timestamp_seconds": 10,
        }))
        .await;
    app.server
        .post("/api/bookmarks")
        .json(&json!({
            "video_id": "vid-b",
            "timestamp_seconds": 20,
        }))
        .await;

    let res = app.server.get("/api/bookmarks/vid-a").await;
    assert!(res.status_code().is_success());
    let body: serde_json::Value = res.json();
    let arr = body.as_array().unwrap();
    assert_eq!(arr.len(), 1);
    assert_eq!(arr[0]["video_id"], "vid-a");
}

#[tokio::test]
async fn timer_lifecycle() {
    let (app, _auth) = boot_with_parent_and_child(AccountType::Child).await;

    // Initially nothing.
    let res = app.server.get("/api/timer").await;
    assert!(res.status_code().is_success());
    let body: serde_json::Value = res.json();
    assert!(body.is_null());

    // Set a 30-min timer.
    let res = app
        .server
        .post("/api/timer")
        .json(&json!({ "type": "minutes", "minutes": 30 }))
        .await;
    assert!(res.status_code().is_success());
    let body: serde_json::Value = res.json();
    assert_eq!(body["timer_type"], "minutes");
    assert_eq!(body["minutes_remaining"], 30);

    // Get returns the active row.
    let res = app.server.get("/api/timer").await;
    let body: serde_json::Value = res.json();
    assert_eq!(body["minutes_remaining"], 30);

    // Cancel.
    let res = app.server.delete("/api/timer").await;
    assert_eq!(res.status_code(), StatusCode::NO_CONTENT);
    let res = app.server.get("/api/timer").await;
    let body: serde_json::Value = res.json();
    assert!(body.is_null());
}

#[tokio::test]
async fn timer_after_video_type() {
    let (app, _auth) = boot_with_parent_and_child(AccountType::Child).await;
    let res = app
        .server
        .post("/api/timer")
        .json(&json!({ "type": "after_video" }))
        .await;
    assert!(res.status_code().is_success());
    let body: serde_json::Value = res.json();
    assert_eq!(body["timer_type"], "after_video");
}

#[tokio::test]
async fn timer_rejects_invalid_type() {
    let (app, _auth) = boot_with_parent_and_child(AccountType::Child).await;
    let res = app
        .server
        .post("/api/timer")
        .json(&json!({ "type": "weird" }))
        .await;
    assert_eq!(res.status_code(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn timer_rejects_out_of_range_minutes() {
    let (app, _auth) = boot_with_parent_and_child(AccountType::Child).await;
    let res = app
        .server
        .post("/api/timer")
        .json(&json!({ "type": "minutes", "minutes": 999 }))
        .await;
    assert_eq!(res.status_code(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn child_can_read_own_settings() {
    let (app, _auth) = boot_with_parent_and_child(AccountType::Child).await;
    let res = app.server.get("/api/children/me/settings").await;
    assert!(res.status_code().is_success());
    let body: serde_json::Value = res.json();
    // Defaults established by `ensure_settings_row`. The handler
    // serialises the boolean as `true`/`false`, not the underlying
    // SQLite 1/0 integer.
    assert!(body["downloads_enabled"].as_bool().unwrap_or(false));
}

#[tokio::test]
async fn downloads_list_is_empty_initially() {
    let (app, _auth) = boot_with_parent_and_child(AccountType::Child).await;
    let res = app.server.get("/api/downloads").await;
    assert!(res.status_code().is_success());
    let body: serde_json::Value = res.json();
    assert!(body.as_array().unwrap().is_empty());
}
