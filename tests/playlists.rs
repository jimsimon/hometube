//! Child playlists CRUD coverage.
//!
//! `POST /api/playlists/:id/videos` resolves the video against the
//! YouTube Data API, which we don't reach in tests. Every other route
//! in the file works against the `child_playlists` /
//! `child_playlist_videos` tables and can be driven entirely through
//! the harness.

mod common;

use axum::http::StatusCode;
use common::boot_with_parent_and_child;
use hometube::models::account::AccountType;
use serde_json::json;

async fn create_playlist(app: &common::TestApp, title: &str) -> i64 {
    let res = app
        .server
        .post("/api/playlists")
        .json(&json!({ "title": title, "description": "" }))
        .await;
    let body: serde_json::Value = res.json();
    body["id"].as_i64().unwrap()
}

#[tokio::test]
async fn create_with_description() {
    let (app, _auth) = boot_with_parent_and_child(AccountType::Child).await;
    let res = app
        .server
        .post("/api/playlists")
        .json(&json!({ "title": "Mix", "description": "fun stuff" }))
        .await;
    assert!(res.status_code().is_success());
    let body: serde_json::Value = res.json();
    assert_eq!(body["title"], "Mix");
    assert_eq!(body["description"], "fun stuff");
    assert_eq!(body["sync_status"], "pending_create");
}

#[tokio::test]
async fn detail_returns_videos_in_position_order() {
    let (app, auth) = boot_with_parent_and_child(AccountType::Child).await;
    let id = create_playlist(&app, "PL").await;

    // Seed videos directly so we don't have to call YouTube.
    for (pos, vid) in [(0, "vid-a"), (1, "vid-b")] {
        sqlx::query(
            "INSERT INTO child_playlist_videos \
                (playlist_id, video_id, video_title, position) \
             VALUES (?, ?, 'T', ?)",
        )
        .bind(id)
        .bind(vid)
        .bind(pos)
        .execute(&app.pool)
        .await
        .unwrap();
    }

    let res = app.server.get(&format!("/api/playlists/{id}")).await;
    assert_eq!(res.status_code(), StatusCode::OK);
    let body: serde_json::Value = res.json();
    let videos = body["videos"].as_array().unwrap();
    assert_eq!(videos.len(), 2);
    assert_eq!(videos[0]["video_id"], "vid-a");
    assert_eq!(videos[1]["video_id"], "vid-b");
    let _ = auth;
}

#[tokio::test]
async fn update_changes_title() {
    let (app, _auth) = boot_with_parent_and_child(AccountType::Child).await;
    let id = create_playlist(&app, "Original").await;
    let res = app
        .server
        .put(&format!("/api/playlists/{id}"))
        .json(&json!({ "title": "Renamed" }))
        .await;
    assert!(res.status_code().is_success());
    let body: serde_json::Value = res.json();
    assert_eq!(body["title"], "Renamed");
}

#[tokio::test]
async fn update_rejects_empty_title() {
    let (app, _auth) = boot_with_parent_and_child(AccountType::Child).await;
    let id = create_playlist(&app, "Original").await;
    let res = app
        .server
        .put(&format!("/api/playlists/{id}"))
        .json(&json!({ "title": "   " }))
        .await;
    assert_eq!(res.status_code(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn delete_soft_deletes() {
    let (app, _auth) = boot_with_parent_and_child(AccountType::Child).await;
    let id = create_playlist(&app, "Doomed").await;
    let res = app.server.delete(&format!("/api/playlists/{id}")).await;
    assert_eq!(res.status_code(), StatusCode::NO_CONTENT);

    // The list endpoint now hides it.
    let res = app.server.get("/api/playlists").await;
    let body: serde_json::Value = res.json();
    assert!(
        body.as_array()
            .unwrap()
            .iter()
            .all(|p| p["id"].as_i64() != Some(id)),
        "deleted playlist must not appear in /api/playlists"
    );
}

#[tokio::test]
async fn remove_video_succeeds() {
    let (app, _auth) = boot_with_parent_and_child(AccountType::Child).await;
    let id = create_playlist(&app, "PL").await;
    sqlx::query(
        "INSERT INTO child_playlist_videos \
            (playlist_id, video_id, video_title, position) \
         VALUES (?, 'vid-a', 'T', 0)",
    )
    .bind(id)
    .execute(&app.pool)
    .await
    .unwrap();

    let res = app
        .server
        .delete(&format!("/api/playlists/{id}/videos/vid-a"))
        .await;
    assert_eq!(res.status_code(), StatusCode::NO_CONTENT);
}

#[tokio::test]
async fn remove_video_404_when_missing() {
    let (app, _auth) = boot_with_parent_and_child(AccountType::Child).await;
    let id = create_playlist(&app, "PL").await;
    let res = app
        .server
        .delete(&format!("/api/playlists/{id}/videos/nope"))
        .await;
    assert_eq!(res.status_code(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn reorder_changes_positions() {
    let (app, _auth) = boot_with_parent_and_child(AccountType::Child).await;
    let id = create_playlist(&app, "PL").await;
    for (pos, vid) in [(0, "vid-a"), (1, "vid-b"), (2, "vid-c")] {
        sqlx::query(
            "INSERT INTO child_playlist_videos \
                (playlist_id, video_id, video_title, position) \
             VALUES (?, ?, 'T', ?)",
        )
        .bind(id)
        .bind(vid)
        .bind(pos)
        .execute(&app.pool)
        .await
        .unwrap();
    }

    let res = app
        .server
        .put(&format!("/api/playlists/{id}/videos/reorder"))
        .json(&json!({ "video_ids": ["vid-c", "vid-a", "vid-b"] }))
        .await;
    assert!(res.status_code().is_success());
    let body: serde_json::Value = res.json();
    let arr = body.as_array().unwrap();
    assert_eq!(arr[0]["video_id"], "vid-c");
    assert_eq!(arr[1]["video_id"], "vid-a");
    assert_eq!(arr[2]["video_id"], "vid-b");
}

#[tokio::test]
async fn detail_404_for_missing_playlist() {
    let (app, _auth) = boot_with_parent_and_child(AccountType::Child).await;
    let res = app.server.get("/api/playlists/9999").await;
    assert_eq!(res.status_code(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn cannot_modify_other_childs_playlist() {
    // Two children: child_a creates a playlist, child_b tries to rename it.
    let (app, _parent_auth) = boot_with_parent_and_child(AccountType::Parent).await;
    let parent_id = app.parent_id.unwrap();
    let child_a = app.child_id.unwrap();
    let child_b = common::insert_account(
        &app.pool,
        "google-child-2",
        "child2@example.test",
        "Child Two",
        AccountType::Child,
    )
    .await;
    let _ = parent_id;

    // child_a creates a playlist.
    let auth_a = common::mint_session_cookie(&app, child_a).await;
    let res = app
        .server
        .post("/api/playlists")
        .clear_cookies()
        .add_cookie(tower_cookies::cookie::Cookie::new(
            auth_a.name,
            auth_a.value,
        ))
        .json(&json!({ "title": "A's Mix", "description": "" }))
        .await;
    let body: serde_json::Value = res.json();
    let id = body["id"].as_i64().unwrap();

    // child_b tries to rename it → 404.
    let auth_b = common::mint_session_cookie(&app, child_b).await;
    let res = app
        .server
        .put(&format!("/api/playlists/{id}"))
        .clear_cookies()
        .add_cookie(tower_cookies::cookie::Cookie::new(
            auth_b.name,
            auth_b.value,
        ))
        .json(&json!({ "title": "Stolen" }))
        .await;
    assert_eq!(res.status_code(), StatusCode::NOT_FOUND);
}
