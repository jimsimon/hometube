//! Extended feed route tests — covers continue-watching, up-next from
//! various contexts, and edge cases.

mod common;

use axum::http::StatusCode;
use common::boot_with_parent_and_child;
use hometube::models::account::AccountType;

// ---------------------------------------------------------------------------
// Continue watching
// ---------------------------------------------------------------------------

#[tokio::test]
async fn continue_watching_returns_seeded_history_with_access_check() {
    let (app, auth) = boot_with_parent_and_child(AccountType::Child).await;
    let child_id = auth.account_id;
    let parent_id = app.parent_id.unwrap();

    // Seed watch history.
    sqlx::query(
        "INSERT INTO watch_history (child_account_id, video_id, video_title, video_thumbnail_url, \
         channel_title, duration_seconds, progress_seconds, last_watched_at) \
         VALUES (?, 'vid-ok', 'Allowed', NULL, 'Ch', 300, 120, 1000)",
    )
    .bind(child_id)
    .execute(&app.pool)
    .await
    .unwrap();

    // Allowlist the video.
    sqlx::query(
        "INSERT INTO allowlisted_videos (child_account_id, video_id, video_title, added_by) \
         VALUES (?, 'vid-ok', 'Allowed', ?)",
    )
    .bind(child_id)
    .bind(parent_id)
    .execute(&app.pool)
    .await
    .unwrap();

    // Seed a non-allowlisted video in history.
    sqlx::query(
        "INSERT INTO watch_history (child_account_id, video_id, video_title, video_thumbnail_url, \
         channel_title, duration_seconds, progress_seconds, last_watched_at) \
         VALUES (?, 'vid-noallow', 'Hidden', NULL, 'Ch2', 200, 50, 999)",
    )
    .bind(child_id)
    .execute(&app.pool)
    .await
    .unwrap();

    let res = app.server.get("/api/feed/continue-watching").await;
    assert_eq!(res.status_code(), StatusCode::OK);
    let body: serde_json::Value = res.json();
    let arr = body.as_array().unwrap();
    // Only the allowlisted video appears.
    assert_eq!(arr.len(), 1);
    assert_eq!(arr[0]["video_id"], "vid-ok");
    assert_eq!(arr[0]["progress_seconds"], 120);
}

#[tokio::test]
async fn continue_watching_drops_effectively_finished_videos() {
    let (app, auth) = boot_with_parent_and_child(AccountType::Child).await;
    let child_id = auth.account_id;
    let parent_id = app.parent_id.unwrap();

    // Three watched + allowlisted videos:
    //   vid-done-tail   — within 15s of the end → finished
    //   vid-done-ratio  — past the 95% completion threshold → finished
    //   vid-partial     — half-watched → still in continue-watching
    // Plus vid-no-dur with NULL duration which must NOT be auto-dropped.
    let seeds = [
        ("vid-done-tail", Some(300i64), 290i64, 1003i64),
        ("vid-done-ratio", Some(1000i64), 960i64, 1002i64),
        ("vid-partial", Some(600i64), 120i64, 1001i64),
        ("vid-no-dur", None, 30i64, 1000i64),
    ];
    for (video_id, duration, progress, ts) in seeds {
        sqlx::query(
            "INSERT INTO watch_history (child_account_id, video_id, video_title, \
             video_thumbnail_url, channel_title, duration_seconds, progress_seconds, \
             last_watched_at) VALUES (?, ?, 'T', NULL, 'Ch', ?, ?, ?)",
        )
        .bind(child_id)
        .bind(video_id)
        .bind(duration)
        .bind(progress)
        .bind(ts)
        .execute(&app.pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO allowlisted_videos (child_account_id, video_id, video_title, added_by) \
             VALUES (?, ?, 'T', ?)",
        )
        .bind(child_id)
        .bind(video_id)
        .bind(parent_id)
        .execute(&app.pool)
        .await
        .unwrap();
    }

    let res = app.server.get("/api/feed/continue-watching").await;
    assert_eq!(res.status_code(), StatusCode::OK);
    let body: serde_json::Value = res.json();
    let ids: Vec<&str> = body
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v["video_id"].as_str().unwrap())
        .collect();
    assert!(
        ids.contains(&"vid-partial"),
        "half-watched video must remain, got {ids:?}"
    );
    assert!(
        ids.contains(&"vid-no-dur"),
        "rows with NULL duration must not be auto-finished, got {ids:?}"
    );
    assert!(
        !ids.contains(&"vid-done-tail"),
        "video within tail window must be dropped, got {ids:?}"
    );
    assert!(
        !ids.contains(&"vid-done-ratio"),
        "video past 95% completion must be dropped, got {ids:?}"
    );
}

#[tokio::test]
async fn continue_watching_includes_channel_allowlisted_videos() {
    // Regression: continue-watching used to pass `channel_id=None` to
    // `can_child_view`, so only individually allowlisted videos
    // survived the filter. Videos surfaced via an allowlisted channel
    // were silently dropped from the row.
    let (app, auth) = boot_with_parent_and_child(AccountType::Child).await;
    let child_id = auth.account_id;
    let parent_id = app.parent_id.unwrap();

    // Allowlist a channel, NOT the individual video.
    sqlx::query(
        "INSERT INTO allowlisted_channels (child_account_id, channel_id, channel_title, added_by) \
         VALUES (?, 'ch-allow', 'My Channel', ?)",
    )
    .bind(child_id)
    .bind(parent_id)
    .execute(&app.pool)
    .await
    .unwrap();

    // (a) New-style row: channel_id stored directly on watch_history.
    sqlx::query(
        "INSERT INTO watch_history (child_account_id, video_id, video_title, video_thumbnail_url, \
         channel_title, channel_id, duration_seconds, progress_seconds, last_watched_at) \
         VALUES (?, 'vid-new', 'Direct', NULL, 'My Channel', 'ch-allow', 600, 120, 1002)",
    )
    .bind(child_id)
    .execute(&app.pool)
    .await
    .unwrap();

    // (b) Legacy row: channel_id NULL but resolvable via
    // feed_source_items (the refresher cache).
    sqlx::query(
        "INSERT INTO feed_sources (kind, source_id, title) \
         VALUES ('channel', 'ch-allow', 'My Channel')",
    )
    .execute(&app.pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO feed_source_items (kind, source_id, video_id, title, channel_id, \
         channel_title, thumbnail_url, published_at, fetched_at) \
         VALUES ('channel', 'ch-allow', 'vid-legacy', 'Legacy', 'ch-allow', 'My Channel', NULL, 1, 1)",
    )
    .execute(&app.pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO watch_history (child_account_id, video_id, video_title, video_thumbnail_url, \
         channel_title, duration_seconds, progress_seconds, last_watched_at) \
         VALUES (?, 'vid-legacy', 'Legacy', NULL, 'My Channel', 600, 90, 1001)",
    )
    .bind(child_id)
    .execute(&app.pool)
    .await
    .unwrap();

    let res = app.server.get("/api/feed/continue-watching").await;
    assert_eq!(res.status_code(), StatusCode::OK);
    let body: serde_json::Value = res.json();
    let ids: Vec<&str> = body
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v["video_id"].as_str().unwrap())
        .collect();
    assert!(
        ids.contains(&"vid-new"),
        "channel-allowlisted video with stored channel_id must surface, got {ids:?}"
    );
    assert!(
        ids.contains(&"vid-legacy"),
        "legacy row with NULL channel_id must resolve via feed_source_items, got {ids:?}"
    );
}

#[tokio::test]
async fn continue_watching_empty_for_fresh_child() {
    let (app, _auth) = boot_with_parent_and_child(AccountType::Child).await;
    let res = app.server.get("/api/feed/continue-watching").await;
    assert_eq!(res.status_code(), StatusCode::OK);
    let body: serde_json::Value = res.json();
    assert!(body.as_array().unwrap().is_empty());
}

// ---------------------------------------------------------------------------
// Up-next
// ---------------------------------------------------------------------------

#[tokio::test]
async fn up_next_from_playlist_returns_videos() {
    let (app, auth) = boot_with_parent_and_child(AccountType::Child).await;
    let child_id = auth.account_id;
    let parent_id = app.parent_id.unwrap();

    // Create a child playlist.
    let pl_id: i64 = sqlx::query_scalar(
        "INSERT INTO child_playlists (child_account_id, title, is_own) \
         VALUES (?, 'My Mix', 1) RETURNING id",
    )
    .bind(child_id)
    .fetch_one(&app.pool)
    .await
    .unwrap();

    // Add videos.
    for (pos, vid) in [(0, "vid-a"), (1, "vid-b"), (2, "vid-c")] {
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
    }

    // Allowlist all videos.
    for vid in ["vid-a", "vid-b", "vid-c"] {
        sqlx::query(
            "INSERT INTO allowlisted_videos (child_account_id, video_id, video_title, added_by) \
             VALUES (?, ?, 'T', ?)",
        )
        .bind(child_id)
        .bind(vid)
        .bind(parent_id)
        .execute(&app.pool)
        .await
        .unwrap();
    }

    let url = format!("/api/feed/up-next?from=playlist:{pl_id}&current_video=vid-a");
    let res = app.server.get(&url).await;
    assert_eq!(res.status_code(), StatusCode::OK);
    let body: serde_json::Value = res.json();
    let arr = body.as_array().unwrap();
    // vid-a is excluded (current_video), vid-b and vid-c remain.
    assert_eq!(arr.len(), 2);
    assert_eq!(arr[0]["video_id"], "vid-b");
    assert_eq!(arr[1]["video_id"], "vid-c");
}

#[tokio::test]
async fn up_next_without_from_returns_empty_for_no_api_key() {
    // The default test setup seeds a fake API key which will fail when
    // trying to call YouTube, so new_videos falls back to empty.
    let (app, _auth) = boot_with_parent_and_child(AccountType::Child).await;
    let res = app.server.get("/api/feed/up-next").await;
    assert_eq!(res.status_code(), StatusCode::OK);
    let body: serde_json::Value = res.json();
    assert!(body.as_array().unwrap().is_empty());
}

#[tokio::test]
async fn up_next_with_unknown_playlist_returns_empty() {
    let (app, _auth) = boot_with_parent_and_child(AccountType::Child).await;
    let res = app
        .server
        .get("/api/feed/up-next?from=playlist:99999")
        .await;
    assert_eq!(res.status_code(), StatusCode::OK);
    let body: serde_json::Value = res.json();
    assert!(body.as_array().unwrap().is_empty());
}

#[tokio::test]
async fn up_next_with_limit() {
    let (app, auth) = boot_with_parent_and_child(AccountType::Child).await;
    let child_id = auth.account_id;
    let parent_id = app.parent_id.unwrap();

    let pl_id: i64 = sqlx::query_scalar(
        "INSERT INTO child_playlists (child_account_id, title, is_own) \
         VALUES (?, 'Many', 1) RETURNING id",
    )
    .bind(child_id)
    .fetch_one(&app.pool)
    .await
    .unwrap();

    for i in 0..5 {
        let vid = format!("vid-{i}");
        sqlx::query(
            "INSERT INTO child_playlist_videos (playlist_id, video_id, video_title, position) \
             VALUES (?, ?, 'T', ?)",
        )
        .bind(pl_id)
        .bind(&vid)
        .bind(i)
        .execute(&app.pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO allowlisted_videos (child_account_id, video_id, video_title, added_by) \
             VALUES (?, ?, 'T', ?)",
        )
        .bind(child_id)
        .bind(&vid)
        .bind(parent_id)
        .execute(&app.pool)
        .await
        .unwrap();
    }

    let url = format!("/api/feed/up-next?from=playlist:{pl_id}&limit=2");
    let res = app.server.get(&url).await;
    assert_eq!(res.status_code(), StatusCode::OK);
    let body: serde_json::Value = res.json();
    assert_eq!(body.as_array().unwrap().len(), 2);
}

#[tokio::test]
async fn up_next_from_playlist_cursors_after_current_video() {
    // With current_video set to a middle item, the response should
    // continue from the next position and wrap around — never resurface
    // the same prefix every time.
    let (app, auth) = boot_with_parent_and_child(AccountType::Child).await;
    let child_id = auth.account_id;
    let parent_id = app.parent_id.unwrap();

    let pl_id: i64 = sqlx::query_scalar(
        "INSERT INTO child_playlists (child_account_id, title, is_own) \
         VALUES (?, 'Cursor', 1) RETURNING id",
    )
    .bind(child_id)
    .fetch_one(&app.pool)
    .await
    .unwrap();

    for (pos, vid) in [(0, "v0"), (1, "v1"), (2, "v2"), (3, "v3")] {
        sqlx::query(
            "INSERT INTO child_playlist_videos (playlist_id, video_id, video_title, position) \
             VALUES (?, ?, 'T', ?)",
        )
        .bind(pl_id)
        .bind(vid)
        .bind(pos)
        .execute(&app.pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO allowlisted_videos (child_account_id, video_id, video_title, added_by) \
             VALUES (?, ?, 'T', ?)",
        )
        .bind(child_id)
        .bind(vid)
        .bind(parent_id)
        .execute(&app.pool)
        .await
        .unwrap();
    }

    let url = format!("/api/feed/up-next?from=playlist:{pl_id}&current_video=v2");
    let res = app.server.get(&url).await;
    assert_eq!(res.status_code(), StatusCode::OK);
    let body: serde_json::Value = res.json();
    let arr = body.as_array().unwrap();
    // Cursor at v2 → next is v3, then wrap to v0, v1.
    let ids: Vec<&str> = arr
        .iter()
        .map(|v| v["video_id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, vec!["v3", "v0", "v1"]);
}

#[tokio::test]
async fn up_next_playlist_ignores_watch_history() {
    // Playlist contexts preserve order even when items have been
    // watched before — users explicitly opened the playlist.
    let (app, auth) = boot_with_parent_and_child(AccountType::Child).await;
    let child_id = auth.account_id;
    let parent_id = app.parent_id.unwrap();

    let pl_id: i64 = sqlx::query_scalar(
        "INSERT INTO child_playlists (child_account_id, title, is_own) \
         VALUES (?, 'Watched', 1) RETURNING id",
    )
    .bind(child_id)
    .fetch_one(&app.pool)
    .await
    .unwrap();

    for (pos, vid) in [(0, "w-a"), (1, "w-b")] {
        sqlx::query(
            "INSERT INTO child_playlist_videos (playlist_id, video_id, video_title, position) \
             VALUES (?, ?, 'T', ?)",
        )
        .bind(pl_id)
        .bind(vid)
        .bind(pos)
        .execute(&app.pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO allowlisted_videos (child_account_id, video_id, video_title, added_by) \
             VALUES (?, ?, 'T', ?)",
        )
        .bind(child_id)
        .bind(vid)
        .bind(parent_id)
        .execute(&app.pool)
        .await
        .unwrap();
    }

    // Mark w-b as already watched.
    sqlx::query(
        "INSERT INTO watch_history (child_account_id, video_id, video_title, video_thumbnail_url, \
         channel_title, duration_seconds, progress_seconds, last_watched_at) \
         VALUES (?, 'w-b', 'T', NULL, NULL, 100, 100, 1)",
    )
    .bind(child_id)
    .execute(&app.pool)
    .await
    .unwrap();

    let url = format!("/api/feed/up-next?from=playlist:{pl_id}&current_video=w-a");
    let body: serde_json::Value = app.server.get(&url).await.json();
    let ids: Vec<&str> = body
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v["video_id"].as_str().unwrap())
        .collect();
    // w-b is still present despite being in watch_history.
    assert_eq!(ids, vec!["w-b"]);
}

// ---------------------------------------------------------------------------
// New videos feed
// ---------------------------------------------------------------------------

#[tokio::test]
async fn new_videos_returns_empty_with_no_allowlist() {
    let (app, _auth) = boot_with_parent_and_child(AccountType::Child).await;
    let res = app.server.get("/api/feed/new-videos").await;
    assert_eq!(res.status_code(), StatusCode::OK);
    let body: serde_json::Value = res.json();
    assert!(body.as_array().unwrap().is_empty());
}

#[tokio::test]
async fn up_next_from_youtube_playlist_id() {
    let (app, auth) = boot_with_parent_and_child(AccountType::Child).await;
    let child_id = auth.account_id;
    let parent_id = app.parent_id.unwrap();

    // Create a YouTube-sourced playlist.
    let pl_id: i64 = sqlx::query_scalar(
        "INSERT INTO child_playlists (child_account_id, youtube_playlist_id, title, is_own) \
         VALUES (?, 'YT_PL_UP', 'YT Up', 0) RETURNING id",
    )
    .bind(child_id)
    .fetch_one(&app.pool)
    .await
    .unwrap();

    sqlx::query(
        "INSERT INTO child_playlist_videos (playlist_id, video_id, video_title, position) \
         VALUES (?, 'yt-v1', 'YT V1', 0)",
    )
    .bind(pl_id)
    .execute(&app.pool)
    .await
    .unwrap();

    sqlx::query(
        "INSERT INTO allowlisted_videos (child_account_id, video_id, video_title, added_by) \
         VALUES (?, 'yt-v1', 'YT V1', ?)",
    )
    .bind(child_id)
    .bind(parent_id)
    .execute(&app.pool)
    .await
    .unwrap();

    // Use the youtube_playlist_id as the context.
    let res = app
        .server
        .get("/api/feed/up-next?from=playlist:YT_PL_UP")
        .await;
    assert_eq!(res.status_code(), StatusCode::OK);
    let body: serde_json::Value = res.json();
    let arr = body.as_array().unwrap();
    assert_eq!(arr.len(), 1);
    assert_eq!(arr[0]["video_id"], "yt-v1");
}
