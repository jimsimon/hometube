//! Child-side allowlist-bounded search.
//!
//! `parent_search` hits the YouTube Data API directly so we don't
//! exercise it. `child_search` queries our own tables (allowlist,
//! subscriptions, watch history, playlists), so we can drive it
//! against a small fixture and assert the results / `search_log` row.

mod common;

use axum::http::StatusCode;
use common::boot_with_parent_and_child;
use hometube::models::account::AccountType;

#[tokio::test]
async fn child_search_requires_q() {
    let (app, _auth) = boot_with_parent_and_child(AccountType::Child).await;
    // Empty `q` → 400.
    let res = app.server.get("/api/search?q=").await;
    assert_eq!(res.status_code(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn child_search_returns_buckets_and_logs() {
    let (app, auth) = boot_with_parent_and_child(AccountType::Child).await;
    let child_id = auth.account_id;
    let parent_id = app.parent_id.unwrap();

    sqlx::query(
        "INSERT INTO allowlisted_channels (child_account_id, channel_id, channel_title, added_by) \
         VALUES (?, 'chan-1', 'Cooking with Kids', ?)",
    )
    .bind(child_id)
    .bind(parent_id)
    .execute(&app.pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO allowlisted_videos (child_account_id, video_id, video_title, added_by) \
         VALUES (?, 'vid-1', 'Cooking 101', ?)",
    )
    .bind(child_id)
    .bind(parent_id)
    .execute(&app.pool)
    .await
    .unwrap();

    let res = app.server.get("/api/search?q=Cooking&type=all").await;
    assert_eq!(res.status_code(), StatusCode::OK);
    let body: serde_json::Value = res.json();
    assert_eq!(body["q"], "Cooking");
    let chans = body["results"]["channels"].as_array().unwrap();
    assert!(!chans.is_empty());
    let videos = body["results"]["videos"].as_array().unwrap();
    assert!(!videos.is_empty());

    // search_log gets a row.
    let count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM search_log WHERE child_account_id = ?",
    )
    .bind(child_id)
    .fetch_one(&app.pool)
    .await
    .unwrap();
    assert_eq!(count, 1);
}

#[tokio::test]
async fn child_search_kind_filter_returns_only_one_bucket() {
    let (app, auth) = boot_with_parent_and_child(AccountType::Child).await;
    let child_id = auth.account_id;
    let parent_id = app.parent_id.unwrap();

    sqlx::query(
        "INSERT INTO allowlisted_videos (child_account_id, video_id, video_title, added_by) \
         VALUES (?, 'vid-1', 'Hello World', ?)",
    )
    .bind(child_id)
    .bind(parent_id)
    .execute(&app.pool)
    .await
    .unwrap();

    let res = app.server.get("/api/search?q=Hello&type=video").await;
    assert_eq!(res.status_code(), StatusCode::OK);
    let body: serde_json::Value = res.json();
    assert!(body["results"]["channels"].as_array().unwrap().is_empty());
    assert!(body["results"]["playlists"].as_array().unwrap().is_empty());
    assert!(!body["results"]["videos"].as_array().unwrap().is_empty());
}

#[tokio::test]
async fn child_search_returns_empty_for_no_match() {
    let (app, _auth) = boot_with_parent_and_child(AccountType::Child).await;
    let res = app.server.get("/api/search?q=zzznonexistent").await;
    assert_eq!(res.status_code(), StatusCode::OK);
    let body: serde_json::Value = res.json();
    assert!(body["results"]["channels"].as_array().unwrap().is_empty());
    assert!(body["results"]["playlists"].as_array().unwrap().is_empty());
    assert!(body["results"]["videos"].as_array().unwrap().is_empty());
}

#[tokio::test]
async fn up_next_returns_empty_with_no_state() {
    let (app, _auth) = boot_with_parent_and_child(AccountType::Child).await;
    let res = app.server.get("/api/feed/up-next").await;
    assert_eq!(res.status_code(), StatusCode::OK);
    let body: serde_json::Value = res.json();
    assert!(body.is_array());
}

#[tokio::test]
async fn up_next_with_playlist_context() {
    let (app, auth) = boot_with_parent_and_child(AccountType::Child).await;
    let child_id = auth.account_id;
    let parent_id = app.parent_id.unwrap();

    // Create a child playlist with two videos. Both videos are
    // allowlisted so they pass `can_child_view`.
    let pl_id: i64 = sqlx::query_scalar(
        "INSERT INTO child_playlists (child_account_id, title, source) \
         VALUES (?, 'Test PL', 'app') RETURNING id",
    )
    .bind(child_id)
    .fetch_one(&app.pool)
    .await
    .unwrap();
    for (pos, vid) in [(0, "vid-a"), (1, "vid-b")] {
        sqlx::query(
            "INSERT INTO child_playlist_videos (playlist_id, video_id, video_title, position) \
             VALUES (?, ?, 'Title', ?)",
        )
        .bind(pl_id)
        .bind(vid)
        .bind(pos)
        .execute(&app.pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO allowlisted_videos (child_account_id, video_id, video_title, added_by) \
             VALUES (?, ?, 'Title', ?)",
        )
        .bind(child_id)
        .bind(vid)
        .bind(parent_id)
        .execute(&app.pool)
        .await
        .unwrap();
    }

    let res = app
        .server
        .get(&format!(
            "/api/feed/up-next?from=playlist:{pl_id}&current_video=vid-a"
        ))
        .await;
    assert_eq!(res.status_code(), StatusCode::OK);
    let body: serde_json::Value = res.json();
    let arr = body.as_array().unwrap();
    // Excludes the current video, leaving "vid-b".
    assert!(arr.iter().any(|i| i["video_id"] == "vid-b"));
    assert!(arr.iter().all(|i| i["video_id"] != "vid-a"));
}
