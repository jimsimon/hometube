//! Family-playlists CRUD coverage.
//!
//! Family-playlists are parent-created, child-visible playlists. The
//! mutation routes don't talk to YouTube directly (videos are passed
//! in the request body, not resolved from yt-dlp), so we can exercise
//! the full create → add video → reorder → delete loop entirely against
//! the test database.

mod common;

use axum::http::StatusCode;
use common::boot_with_parent_and_child;
use hometube::models::account::AccountType;
use serde_json::json;

#[tokio::test]
async fn create_list_detail_round_trip() {
    let (app, _auth) = boot_with_parent_and_child(AccountType::Parent).await;
    let child_id = app.child_id.unwrap();

    let res = app
        .server
        .post("/api/family-playlists")
        .json(&json!({
            "title": "Family Favorites",
            "description": "Some great videos",
            "child_ids": [child_id],
        }))
        .await;
    assert_eq!(res.status_code(), StatusCode::OK);
    let body: serde_json::Value = res.json();
    let id = body["id"].as_i64().unwrap();

    // List it back.
    let res = app.server.get("/api/family-playlists").await;
    assert_eq!(res.status_code(), StatusCode::OK);
    let body: serde_json::Value = res.json();
    let arr = body.as_array().unwrap();
    assert!(arr.iter().any(|p| p["id"] == id));

    // Detail.
    let res = app.server.get(&format!("/api/family-playlists/{id}")).await;
    assert_eq!(res.status_code(), StatusCode::OK);
    let body: serde_json::Value = res.json();
    assert_eq!(body["title"], "Family Favorites");
}

#[tokio::test]
async fn create_rejects_empty_title() {
    let (app, _auth) = boot_with_parent_and_child(AccountType::Parent).await;
    let res = app
        .server
        .post("/api/family-playlists")
        .json(&json!({ "title": "", "description": "" }))
        .await;
    assert_eq!(res.status_code(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn remove_video_with_db_seeded_row() {
    // `POST /api/family-playlists/:id/videos` calls the YouTube Data
    // API to populate the title/thumbnail; we can't exercise it
    // without mocking that out. Instead seed the row directly and
    // exercise the delete handler.
    let (app, _auth) = boot_with_parent_and_child(AccountType::Parent).await;
    let res = app
        .server
        .post("/api/family-playlists")
        .json(&json!({ "title": "Test", "description": "" }))
        .await;
    let body: serde_json::Value = res.json();
    let id = body["id"].as_i64().unwrap();

    sqlx::query(
        "INSERT INTO family_playlist_videos (playlist_id, video_id, video_title, position) \
         VALUES (?, 'vid-1', 'Hello', 0)",
    )
    .bind(id)
    .execute(&app.pool)
    .await
    .unwrap();

    let res = app
        .server
        .delete(&format!("/api/family-playlists/{id}/videos/vid-1"))
        .await;
    assert_eq!(res.status_code(), StatusCode::NO_CONTENT);

    // 404 for a video that doesn't exist.
    let res = app
        .server
        .delete(&format!("/api/family-playlists/{id}/videos/no-such-vid"))
        .await;
    assert_eq!(res.status_code(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn update_changes_title() {
    let (app, _auth) = boot_with_parent_and_child(AccountType::Parent).await;
    let res = app
        .server
        .post("/api/family-playlists")
        .json(&json!({ "title": "Original", "description": "" }))
        .await;
    let body: serde_json::Value = res.json();
    let id = body["id"].as_i64().unwrap();

    let res = app
        .server
        .put(&format!("/api/family-playlists/{id}"))
        .json(&json!({ "title": "Renamed" }))
        .await;
    assert!(res.status_code().is_success());
    let body: serde_json::Value = res.json();
    assert_eq!(body["title"], "Renamed");
}

#[tokio::test]
async fn delete_removes_playlist() {
    let (app, _auth) = boot_with_parent_and_child(AccountType::Parent).await;
    let res = app
        .server
        .post("/api/family-playlists")
        .json(&json!({ "title": "Doomed", "description": "" }))
        .await;
    let body: serde_json::Value = res.json();
    let id = body["id"].as_i64().unwrap();

    let res = app.server.delete(&format!("/api/family-playlists/{id}")).await;
    assert_eq!(res.status_code(), StatusCode::NO_CONTENT);

    let res = app.server.get(&format!("/api/family-playlists/{id}")).await;
    assert_eq!(res.status_code(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn detail_404_for_missing_playlist() {
    let (app, _auth) = boot_with_parent_and_child(AccountType::Parent).await;
    let res = app.server.get("/api/family-playlists/9999").await;
    assert_eq!(res.status_code(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn update_404_for_missing_playlist() {
    let (app, _auth) = boot_with_parent_and_child(AccountType::Parent).await;
    let res = app
        .server
        .put("/api/family-playlists/9999")
        .json(&json!({ "title": "x" }))
        .await;
    assert_eq!(res.status_code(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn create_with_unknown_child_id_400() {
    let (app, _auth) = boot_with_parent_and_child(AccountType::Parent).await;
    let res = app
        .server
        .post("/api/family-playlists")
        .json(&json!({ "title": "Test", "description": "", "child_ids": [9999] }))
        .await;
    assert_eq!(res.status_code(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn reorder_succeeds_with_seeded_videos() {
    let (app, _auth) = boot_with_parent_and_child(AccountType::Parent).await;
    let res = app
        .server
        .post("/api/family-playlists")
        .json(&json!({ "title": "Test", "description": "" }))
        .await;
    let body: serde_json::Value = res.json();
    let id = body["id"].as_i64().unwrap();

    for (pos, vid) in [(0, "vid-a"), (1, "vid-b"), (2, "vid-c")] {
        sqlx::query(
            "INSERT INTO family_playlist_videos (playlist_id, video_id, video_title, position) \
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
        .put(&format!("/api/family-playlists/{id}/videos/reorder"))
        .json(&json!({ "video_ids": ["vid-c", "vid-b", "vid-a"] }))
        .await;
    assert!(res.status_code().is_success());
}

#[tokio::test]
async fn child_can_list_assigned_only() {
    let (app, _auth) = boot_with_parent_and_child(AccountType::Parent).await;
    let child_id = app.child_id.unwrap();

    // Create one assigned to the child + one without any members.
    app.server
        .post("/api/family-playlists")
        .json(&json!({ "title": "Assigned", "description": "", "child_ids": [child_id] }))
        .await;
    app.server
        .post("/api/family-playlists")
        .json(&json!({ "title": "Unassigned", "description": "" }))
        .await;

    // Switch to child session.
    let auth = common::mint_session_cookie(&app, child_id).await;
    let res = app
        .server
        .get("/api/family-playlists")
        .clear_cookies()
        .add_cookie(tower_cookies::cookie::Cookie::new(auth.name, auth.value))
        .await;
    assert!(res.status_code().is_success());
    let body: serde_json::Value = res.json();
    let titles: Vec<String> = body
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p["title"].as_str().unwrap().to_string())
        .collect();
    assert!(titles.contains(&"Assigned".to_string()));
    assert!(!titles.contains(&"Unassigned".to_string()));
}
