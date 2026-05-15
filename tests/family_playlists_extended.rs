//! Extended family playlist tests.
//!
//! The family playlist CRUD doesn't require YouTube API calls for most
//! operations (create, list, detail, update, delete, reorder).

mod common;

use axum::http::StatusCode;
use common::boot_with_parent_and_child;
use hometube::models::account::AccountType;
use serde_json::json;

async fn create_family_playlist(app: &common::TestApp, title: &str, child_ids: &[i64]) -> i64 {
    let res = app
        .server
        .post("/api/family-playlists")
        .json(&json!({
            "title": title,
            "child_ids": child_ids
        }))
        .await;
    assert!(
        res.status_code().is_success(),
        "create failed: {}",
        res.status_code()
    );
    let body: serde_json::Value = res.json();
    body["id"].as_i64().unwrap()
}

#[tokio::test]
async fn create_and_list_family_playlist() {
    let (app, _auth) = boot_with_parent_and_child(AccountType::Parent).await;
    let child_id = app.child_id.unwrap();

    let id = create_family_playlist(&app, "Fun Mix", &[child_id]).await;
    assert!(id > 0);

    let res = app.server.get("/api/family-playlists").await;
    assert_eq!(res.status_code(), StatusCode::OK);
    let body: serde_json::Value = res.json();
    let arr = body.as_array().unwrap();
    assert!(arr.iter().any(|p| p["id"].as_i64() == Some(id)));
}

#[tokio::test]
async fn family_playlist_detail() {
    let (app, _auth) = boot_with_parent_and_child(AccountType::Parent).await;
    let child_id = app.child_id.unwrap();
    let id = create_family_playlist(&app, "Detail PL", &[child_id]).await;

    let res = app.server.get(&format!("/api/family-playlists/{id}")).await;
    assert_eq!(res.status_code(), StatusCode::OK);
    let body: serde_json::Value = res.json();
    assert_eq!(body["title"], "Detail PL");
}

#[tokio::test]
async fn family_playlist_update_title() {
    let (app, _auth) = boot_with_parent_and_child(AccountType::Parent).await;
    let child_id = app.child_id.unwrap();
    let id = create_family_playlist(&app, "Old Title", &[child_id]).await;

    let res = app
        .server
        .put(&format!("/api/family-playlists/{id}"))
        .json(&json!({ "title": "New Title" }))
        .await;
    assert!(res.status_code().is_success());
    let body: serde_json::Value = res.json();
    assert_eq!(body["title"], "New Title");
}

#[tokio::test]
async fn family_playlist_delete() {
    let (app, _auth) = boot_with_parent_and_child(AccountType::Parent).await;
    let child_id = app.child_id.unwrap();
    let id = create_family_playlist(&app, "Doomed", &[child_id]).await;

    let res = app
        .server
        .delete(&format!("/api/family-playlists/{id}"))
        .await;
    assert_eq!(res.status_code(), StatusCode::NO_CONTENT);

    let res = app.server.get(&format!("/api/family-playlists/{id}")).await;
    assert_eq!(res.status_code(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn family_playlist_reorder() {
    let (app, _auth) = boot_with_parent_and_child(AccountType::Parent).await;
    let child_id = app.child_id.unwrap();
    let id = create_family_playlist(&app, "Reorder PL", &[child_id]).await;

    // Seed videos directly.
    for (pos, vid) in [(0, "fv-a"), (1, "fv-b"), (2, "fv-c")] {
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
        .json(&json!({ "video_ids": ["fv-c", "fv-a", "fv-b"] }))
        .await;
    assert!(res.status_code().is_success());
}

#[tokio::test]
async fn family_playlist_remove_video() {
    let (app, _auth) = boot_with_parent_and_child(AccountType::Parent).await;
    let child_id = app.child_id.unwrap();
    let id = create_family_playlist(&app, "Remove PL", &[child_id]).await;

    sqlx::query(
        "INSERT INTO family_playlist_videos (playlist_id, video_id, video_title, position) \
         VALUES (?, 'fv-rm', 'Remove Me', 0)",
    )
    .bind(id)
    .execute(&app.pool)
    .await
    .unwrap();

    let res = app
        .server
        .delete(&format!("/api/family-playlists/{id}/videos/fv-rm"))
        .await;
    assert_eq!(res.status_code(), StatusCode::NO_CONTENT);
}

#[tokio::test]
async fn family_playlist_create_rejects_empty_title() {
    let (app, _auth) = boot_with_parent_and_child(AccountType::Parent).await;
    let child_id = app.child_id.unwrap();

    let res = app
        .server
        .post("/api/family-playlists")
        .json(&json!({ "title": "   ", "child_ids": [child_id] }))
        .await;
    assert_eq!(res.status_code(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn family_playlist_child_can_list_assigned() {
    let (app, _auth) = boot_with_parent_and_child(AccountType::Parent).await;
    let child_id = app.child_id.unwrap();
    create_family_playlist(&app, "Assigned", &[child_id]).await;

    // Switch to child to check access.
    let child_auth = common::mint_session_cookie(&app, child_id).await;
    let res = app
        .server
        .get("/api/family-playlists")
        .clear_cookies()
        .add_cookie(tower_cookies::cookie::Cookie::new(
            child_auth.name,
            child_auth.value,
        ))
        .await;
    assert_eq!(res.status_code(), StatusCode::OK);
    let body: serde_json::Value = res.json();
    assert!(!body.as_array().unwrap().is_empty());
}
