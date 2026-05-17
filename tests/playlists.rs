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
async fn list_marks_inbound_youtube_playlists_invisible_until_allowlisted() {
    let (app, _auth) = boot_with_parent_and_child(AccountType::Child).await;
    let child_id = app.child_id.unwrap();

    // Seed a YouTube-sourced playlist that is *not* allowlisted.
    let hidden_id: i64 = sqlx::query_scalar(
        "INSERT INTO child_playlists \
            (child_account_id, youtube_playlist_id, title, is_own, is_deleted) \
         VALUES (?, 'YT_HIDDEN', 'Hidden Playlist', 0, 0) \
         RETURNING id",
    )
    .bind(child_id)
    .fetch_one(&app.pool)
    .await
    .unwrap();

    // And a child-created playlist (always visible).
    let own_id = create_playlist(&app, "Own").await;

    let res = app.server.get("/api/playlists").await;
    let body: serde_json::Value = res.json();
    let arr = body.as_array().unwrap();

    let hidden = arr.iter().find(|p| p["id"].as_i64() == Some(hidden_id));
    let own = arr.iter().find(|p| p["id"].as_i64() == Some(own_id));
    assert!(hidden.is_some());
    assert!(own.is_some());
    assert_eq!(hidden.unwrap()["visible"], false);
    assert_eq!(own.unwrap()["visible"], true);

    // Allowlist the hidden playlist → it flips to visible.
    let parent_id = app.parent_id.unwrap();
    sqlx::query(
        "INSERT INTO allowlisted_playlists \
            (child_account_id, playlist_id, playlist_title, added_by) \
         VALUES (?, 'YT_HIDDEN', 'Hidden Playlist', ?)",
    )
    .bind(child_id)
    .bind(parent_id)
    .execute(&app.pool)
    .await
    .unwrap();

    let res = app.server.get("/api/playlists").await;
    let body: serde_json::Value = res.json();
    let arr = body.as_array().unwrap();
    let hidden = arr.iter().find(|p| p["id"].as_i64() == Some(hidden_id));
    assert_eq!(hidden.unwrap()["visible"], true);
}

#[tokio::test]
async fn detail_filters_inbound_videos_by_allowlist() {
    let (app, _auth) = boot_with_parent_and_child(AccountType::Child).await;
    let child_id = app.child_id.unwrap();
    let parent_id = app.parent_id.unwrap();

    // Allowlist the playlist itself, but not every video that's in it.
    sqlx::query(
        "INSERT INTO allowlisted_playlists \
            (child_account_id, playlist_id, playlist_title, added_by) \
         VALUES (?, 'YT_PL', 'YT Playlist', ?)",
    )
    .bind(child_id)
    .bind(parent_id)
    .execute(&app.pool)
    .await
    .unwrap();

    // Create the YouTube-sourced playlist locally.
    let pl_id: i64 = sqlx::query_scalar(
        "INSERT INTO child_playlists \
            (child_account_id, youtube_playlist_id, title, is_own, is_deleted) \
         VALUES (?, 'YT_PL', 'YT Playlist', 0, 0) \
         RETURNING id",
    )
    .bind(child_id)
    .fetch_one(&app.pool)
    .await
    .unwrap();

    // Add two videos: one will resolve via the playlist allowlist, the
    // other will be blocked.
    for (pos, vid) in [(0, "vid-allowed"), (1, "vid-blocked")] {
        sqlx::query(
            "INSERT INTO child_playlist_videos \
                (playlist_id, video_id, video_title, position) \
             VALUES (?, ?, 'T', ?)",
        )
        .bind(pl_id)
        .bind(vid)
        .bind(pos)
        .execute(&app.pool)
        .await
        .unwrap();
    }

    // Block one video so even the playlist allowlist can't surface it.
    sqlx::query(
        "INSERT INTO blocked_videos (child_account_id, video_id, video_title, blocked_by) \
         VALUES (?, 'vid-blocked', 'T', ?)",
    )
    .bind(child_id)
    .bind(parent_id)
    .execute(&app.pool)
    .await
    .unwrap();

    let res = app.server.get(&format!("/api/playlists/{pl_id}")).await;
    assert_eq!(res.status_code(), StatusCode::OK);
    let body: serde_json::Value = res.json();
    let videos = body["videos"].as_array().unwrap();
    let ids: Vec<&str> = videos
        .iter()
        .map(|v| v["video_id"].as_str().unwrap())
        .collect();
    assert!(
        ids.contains(&"vid-allowed"),
        "allowlisted video should be returned"
    );
    assert!(
        !ids.contains(&"vid-blocked"),
        "blocked video must not be returned"
    );
}

#[tokio::test]
async fn cannot_modify_other_childs_playlist() {
    // Two children: child_a creates a playlist, child_b tries to rename it.
    let (app, _parent_auth) = boot_with_parent_and_child(AccountType::Parent).await;
    let parent_id = app.parent_id.unwrap();
    let child_a = app.child_id.unwrap();
    let child_b = common::insert_account(&app.pool, "Child Two", AccountType::Child).await;
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
