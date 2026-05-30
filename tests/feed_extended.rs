//! Extended feed route tests — covers continue-watching, up-next from
//! various contexts, and edge cases.

mod common;

use axum::http::StatusCode;
use common::{
    allowlist_channel, allowlist_video, boot_with_parent_and_child, seed_channel_video,
    seed_watch_history,
};
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
    seed_watch_history(
        &app.pool,
        child_id,
        "vid-ok",
        Some("Allowed"),
        None,
        Some("Ch"),
        None,
        Some(300),
        120,
        Some(1000),
    )
    .await;

    // Allowlist the video.
    allowlist_video(
        &app.pool,
        child_id,
        parent_id,
        "vid-ok",
        Some("Allowed"),
        None,
    )
    .await;

    // Seed a non-allowlisted video in history.
    seed_watch_history(
        &app.pool,
        child_id,
        "vid-noallow",
        Some("Hidden"),
        None,
        Some("Ch2"),
        None,
        Some(200),
        50,
        Some(999),
    )
    .await;

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
        seed_watch_history(
            &app.pool,
            child_id,
            video_id,
            Some("T"),
            None,
            Some("Ch"),
            None,
            duration,
            progress,
            Some(ts),
        )
        .await;
        allowlist_video(&app.pool, child_id, parent_id, video_id, Some("T"), None).await;
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
    allowlist_channel(
        &app.pool,
        child_id,
        parent_id,
        "ch-allow",
        Some("My Channel"),
    )
    .await;

    // (a) New-style row: video carries channel_id directly via `videos`.
    seed_watch_history(
        &app.pool,
        child_id,
        "vid-new",
        Some("Direct"),
        Some("ch-allow"),
        Some("My Channel"),
        None,
        Some(600),
        120,
        Some(1002),
    )
    .await;

    // (b) Legacy row: `videos.channel_id` NULL on the watch_history
    // entry but resolvable via `channel_videos` (the refresher cache).
    seed_channel_video(
        &app.pool,
        "ch-allow",
        Some("My Channel"),
        "vid-legacy",
        Some("Legacy"),
        Some(1),
        "rss",
    )
    .await;
    seed_watch_history(
        &app.pool,
        child_id,
        "vid-legacy",
        Some("Legacy"),
        None,
        Some("My Channel"),
        None,
        Some(600),
        90,
        Some(1001),
    )
    .await;

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
        "legacy row with NULL channel_id must resolve via channel_videos, got {ids:?}"
    );
}

#[tokio::test]
async fn continue_watching_lazy_backfill_persists_channel_id() {
    // Regression for the chunked CTE backfill in
    // `backfill_video_channel_ids`. The earlier `UPDATE … FROM
    // (VALUES …) AS src(vid, cid) …` form was rejected by SQLite at
    // parse time (no column-list aliases on VALUES table aliases), so
    // the helper silently no-op'd on every call — `tracing::debug!`
    // swallowed the failure and the live response was already
    // populated from the in-memory map. This test re-reads
    // `videos.channel_id` after a feed call to confirm the write-back
    // actually happened, which is the invariant the docstring's
    // "each row pays this lookup at most once" claim depends on.
    let (app, auth) = boot_with_parent_and_child(AccountType::Child).await;
    let child_id = auth.account_id;
    let parent_id = app.parent_id.unwrap();

    allowlist_channel(
        &app.pool,
        child_id,
        parent_id,
        "ch-backfill",
        Some("Backfill Ch"),
    )
    .await;

    // Set up the legacy-stub scenario: channel_videos has the
    // resolved channel_id, videos.channel_id is NULL. This is the
    // shape only the lazy backfill can repair.
    seed_channel_video(
        &app.pool,
        "ch-backfill",
        Some("Backfill Ch"),
        "vid-bf",
        Some("Backfill"),
        Some(1),
        "rss",
    )
    .await;
    seed_watch_history(
        &app.pool,
        child_id,
        "vid-bf",
        Some("Backfill"),
        None,
        Some("Backfill Ch"),
        None,
        Some(600),
        90,
        Some(1001),
    )
    .await;

    // Force videos.channel_id back to NULL: both seed helpers above
    // funnel through `models::video::upsert`, which (correctly)
    // populates channel_id from any source that supplies it. To
    // simulate the legacy-stub shape we need to roll the column
    // back to NULL after the seeds so the lazy backfill has
    // something to repair. This SQL touch bypasses the production
    // upsert because the production helper has no path that NULLs a
    // populated channel_id — that's the schema invariant the lazy
    // backfill exists to enforce in the opposite direction.
    sqlx::query("UPDATE videos SET channel_id = NULL WHERE video_id = ?")
        .bind("vid-bf")
        .execute(&app.pool)
        .await
        .unwrap();

    // Pre-condition: confirm the stub really is NULL before the call.
    let before: Option<String> =
        sqlx::query_scalar("SELECT channel_id FROM videos WHERE video_id = ?")
            .bind("vid-bf")
            .fetch_one(&app.pool)
            .await
            .unwrap();
    assert!(
        before.is_none(),
        "test fixture must start with NULL channel_id, got {before:?}"
    );

    // Trigger the lazy backfill via the continue-watching route.
    let res = app.server.get("/api/feed/continue-watching").await;
    assert_eq!(res.status_code(), StatusCode::OK);

    // Post-condition: the chunked CTE UPDATE must have populated
    // videos.channel_id so the next call short-circuits without
    // re-reading channel_videos. If this assert fires, the SQL is
    // probably wrong again (parse error swallowed by the warn-once
    // path) — check the `tracing::warn!` log emitted by
    // `backfill_video_channel_ids`.
    let after: Option<String> =
        sqlx::query_scalar("SELECT channel_id FROM videos WHERE video_id = ?")
            .bind("vid-bf")
            .fetch_one(&app.pool)
            .await
            .unwrap();
    assert_eq!(
        after.as_deref(),
        Some("ch-backfill"),
        "lazy backfill must persist channel_id to videos, got {after:?}"
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
    seed_watch_history(
        &app.pool,
        child_id,
        "vid-done",
        Some("Done"),
        None,
        Some("Ch"),
        None,
        Some(100),
        100,
        Some(3000),
    )
    .await;
    // In-progress.
    seed_watch_history(
        &app.pool,
        child_id,
        "vid-half",
        Some("Half"),
        None,
        Some("Ch"),
        None,
        Some(100),
        30,
        Some(3001),
    )
    .await;
    for vid in ["vid-done", "vid-half"] {
        allowlist_video(&app.pool, child_id, parent_id, vid, Some("T"), None).await;
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
    seed_watch_history(
        &app.pool,
        child_id,
        "vid-done",
        Some("Done"),
        None,
        Some("Ch"),
        None,
        Some(300),
        300,
        Some(2000),
    )
    .await;

    // In-progress (50%). Should NOT appear.
    seed_watch_history(
        &app.pool,
        child_id,
        "vid-half",
        Some("Half"),
        None,
        Some("Ch"),
        None,
        Some(300),
        150,
        Some(2001),
    )
    .await;

    // Older completed (95% — at the ratio threshold).
    seed_watch_history(
        &app.pool,
        child_id,
        "vid-old",
        Some("Old"),
        None,
        Some("Ch"),
        None,
        Some(100),
        95,
        Some(1500),
    )
    .await;

    // Allowlist all three so access control isn't the filter.
    for vid in ["vid-done", "vid-half", "vid-old"] {
        allowlist_video(&app.pool, child_id, parent_id, vid, Some("T"), None).await;
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
    seed_watch_history(
        &app.pool,
        child_id,
        "vid-revoked",
        Some("Revoked"),
        None,
        Some("Ch"),
        None,
        Some(100),
        100,
        Some(1),
    )
    .await;

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
