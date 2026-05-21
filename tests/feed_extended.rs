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

    // Five watched + allowlisted videos cover the finished-detection
    // edges (tail threshold + ratio threshold combined with `max`):
    //   vid-done-tail        — 300s clip, watched to 290s (≥ tail 285) → finished
    //   vid-short-finished   — 20s clip, watched to 20s (≥ ratio 19) → finished
    //   vid-short-started    — 20s clip, watched to 6s. The bare tail
    //                          rule (15s) would mark this finished;
    //                          the ratio rule keeps it visible.
    //   vid-partial          — 600s clip, half-watched → still listed
    //   vid-no-dur           — NULL duration must NOT be auto-dropped
    let seeds = [
        ("vid-done-tail", Some(300i64), 290i64, 1004i64),
        ("vid-short-finished", Some(20i64), 20i64, 1003i64),
        ("vid-short-started", Some(20i64), 6i64, 1002i64),
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
        ids.contains(&"vid-short-started"),
        "short clips barely begun must not be auto-finished by the tail rule, got {ids:?}"
    );
    assert!(
        !ids.contains(&"vid-done-tail"),
        "long video within tail window must be dropped, got {ids:?}"
    );
    assert!(
        !ids.contains(&"vid-short-finished"),
        "short video fully watched must be dropped, got {ids:?}"
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

#[tokio::test]
async fn continue_watching_excludes_completed_videos() {
    // Effectively-finished videos (per is_effectively_finished:
    // within CONTINUE_TAIL_SECONDS of the end OR ≥CONTINUE_COMPLETION_RATIO
    // of duration, whichever is later) should appear only under
    // "Watch again", never under "Continue watching".
    let (app, auth) = boot_with_parent_and_child(AccountType::Child).await;
    let child_id = auth.account_id;
    let parent_id = app.parent_id.unwrap();

    // Completed.
    sqlx::query(
        "INSERT INTO watch_history (child_account_id, video_id, video_title, video_thumbnail_url, \
         channel_title, duration_seconds, progress_seconds, last_watched_at) \
         VALUES (?, 'vid-done', 'Done', NULL, 'Ch', 100, 100, 3000)",
    )
    .bind(child_id)
    .execute(&app.pool)
    .await
    .unwrap();
    // In-progress.
    sqlx::query(
        "INSERT INTO watch_history (child_account_id, video_id, video_title, video_thumbnail_url, \
         channel_title, duration_seconds, progress_seconds, last_watched_at) \
         VALUES (?, 'vid-half', 'Half', NULL, 'Ch', 100, 30, 3001)",
    )
    .bind(child_id)
    .execute(&app.pool)
    .await
    .unwrap();
    for vid in ["vid-done", "vid-half"] {
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

    let body: serde_json::Value = app.server.get("/api/feed/continue-watching").await.json();
    let ids: Vec<&str> = body
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v["video_id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, vec!["vid-half"]);
}

// ---------------------------------------------------------------------------
// Watch again
// ---------------------------------------------------------------------------

#[tokio::test]
async fn watch_again_returns_only_completed_videos() {
    let (app, auth) = boot_with_parent_and_child(AccountType::Child).await;
    let child_id = auth.account_id;
    let parent_id = app.parent_id.unwrap();

    // Completed video (100% watched).
    sqlx::query(
        "INSERT INTO watch_history (child_account_id, video_id, video_title, video_thumbnail_url, \
         channel_title, duration_seconds, progress_seconds, last_watched_at) \
         VALUES (?, 'vid-done', 'Done', NULL, 'Ch', 300, 300, 2000)",
    )
    .bind(child_id)
    .execute(&app.pool)
    .await
    .unwrap();

    // In-progress (50%). Should NOT appear.
    sqlx::query(
        "INSERT INTO watch_history (child_account_id, video_id, video_title, video_thumbnail_url, \
         channel_title, duration_seconds, progress_seconds, last_watched_at) \
         VALUES (?, 'vid-half', 'Half', NULL, 'Ch', 300, 150, 2001)",
    )
    .bind(child_id)
    .execute(&app.pool)
    .await
    .unwrap();

    // Older completed (95% — at the ratio threshold).
    sqlx::query(
        "INSERT INTO watch_history (child_account_id, video_id, video_title, video_thumbnail_url, \
         channel_title, duration_seconds, progress_seconds, last_watched_at) \
         VALUES (?, 'vid-old', 'Old', NULL, 'Ch', 100, 95, 1500)",
    )
    .bind(child_id)
    .execute(&app.pool)
    .await
    .unwrap();

    // Allowlist all three so access control isn't the filter.
    for vid in ["vid-done", "vid-half", "vid-old"] {
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

    let res = app.server.get("/api/feed/watch-again").await;
    assert_eq!(res.status_code(), StatusCode::OK);
    let body: serde_json::Value = res.json();
    let ids: Vec<&str> = body
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v["video_id"].as_str().unwrap())
        .collect();
    // In-progress excluded; ordered by last_watched_at DESC.
    assert_eq!(ids, vec!["vid-done", "vid-old"]);
}

#[tokio::test]
async fn watch_again_excludes_access_revoked_videos() {
    let (app, auth) = boot_with_parent_and_child(AccountType::Child).await;
    let child_id = auth.account_id;

    // Completed but no allowlist entry — access denied.
    sqlx::query(
        "INSERT INTO watch_history (child_account_id, video_id, video_title, video_thumbnail_url, \
         channel_title, duration_seconds, progress_seconds, last_watched_at) \
         VALUES (?, 'vid-revoked', 'Revoked', NULL, 'Ch', 100, 100, 1)",
    )
    .bind(child_id)
    .execute(&app.pool)
    .await
    .unwrap();

    let res = app.server.get("/api/feed/watch-again").await;
    assert_eq!(res.status_code(), StatusCode::OK);
    let body: serde_json::Value = res.json();
    assert!(body.as_array().unwrap().is_empty());
}

#[tokio::test]
async fn watch_again_empty_for_fresh_child() {
    let (app, _auth) = boot_with_parent_and_child(AccountType::Child).await;
    let res = app.server.get("/api/feed/watch-again").await;
    assert_eq!(res.status_code(), StatusCode::OK);
    let body: serde_json::Value = res.json();
    assert!(body.as_array().unwrap().is_empty());
}

// ---------------------------------------------------------------------------
// Up-next
// ---------------------------------------------------------------------------

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
