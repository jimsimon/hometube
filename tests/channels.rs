//! Channel-route coverage.
//!
//! `GET /api/channels/:channelId` and `…/videos` both end with a YouTube
//! Data API call, but `enforce_channel_access` fires first and 403s
//! when the channel isn't on the child's allowlist or subscriptions.
//! That gate is everything we can verify without touching the network,
//! and it's important coverage — it's the rule that prevents a child
//! from browsing arbitrary channels.

mod common;

use axum::http::StatusCode;
use common::{allowlist_channel, boot_with_parent_and_child, seed_blocked, seed_channel_video};
use hometube::models::account::AccountType;

#[tokio::test]
async fn unrelated_channel_is_403() {
    let (app, _auth) = boot_with_parent_and_child(AccountType::Child).await;
    let res = app.server.get("/api/channels/unknown-channel").await;
    assert_eq!(res.status_code(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn unrelated_channel_videos_is_403() {
    let (app, _auth) = boot_with_parent_and_child(AccountType::Child).await;
    let res = app.server.get("/api/channels/unknown-channel/videos").await;
    assert_eq!(res.status_code(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn allowlisted_channel_passes_access_gate() {
    let (app, auth) = boot_with_parent_and_child(AccountType::Child).await;
    let child_id = auth.account_id;
    let parent_id = app.parent_id.unwrap();

    allowlist_channel(&app.pool, child_id, parent_id, "chan-ok", Some("Cool")).await;

    // After the access gate, the handler tries to call YouTube. We
    // can't reach a real response without a network call — but we can
    // assert we *didn't* short-circuit at the 403 gate.
    let res = app.server.get("/api/channels/chan-ok").await;
    assert_ne!(res.status_code(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn subscribed_channel_also_passes_access_gate() {
    let (app, auth) = boot_with_parent_and_child(AccountType::Child).await;
    let child_id = auth.account_id;

    sqlx::query(
        "INSERT INTO child_subscriptions (child_account_id, channel_id, channel_title) \
         VALUES (?, 'chan-sub', 'Subscribed')",
    )
    .bind(child_id)
    .execute(&app.pool)
    .await
    .unwrap();

    let res = app.server.get("/api/channels/chan-sub").await;
    assert_ne!(res.status_code(), StatusCode::FORBIDDEN);
}

/// `get_channel` returns the locally-stored header metadata
/// (title/thumbnail/description) without any sidecar call, plus a
/// live-computed `video_count` from `channel_videos`.
#[tokio::test]
async fn get_channel_serves_local_header_metadata() {
    let (app, auth) = boot_with_parent_and_child(AccountType::Child).await;
    let child_id = auth.account_id;
    let parent_id = app.parent_id.unwrap();

    allowlist_channel(&app.pool, child_id, parent_id, "UCmeta", Some("Meta")).await;
    // Override the canonical channel row with the richer metadata this
    // test asserts on.
    sqlx::query(
        "UPDATE channels SET channel_title = 'Meta Channel', \
             channel_thumbnail_url = 'https://yt3.googleusercontent.com/x.jpg', \
             description = 'A description.' \
          WHERE channel_id = 'UCmeta'",
    )
    .execute(&app.pool)
    .await
    .unwrap();

    // Seed two videos, one tombstoned, so video_count = 1.
    seed_channel_video(
        &app.pool,
        "UCmeta",
        Some("Meta Channel"),
        "vA",
        Some("A"),
        Some(1700000000),
        "rss",
    )
    .await;
    seed_channel_video(
        &app.pool,
        "UCmeta",
        Some("Meta Channel"),
        "vB",
        Some("B"),
        Some(1700000001),
        "backfill",
    )
    .await;
    sqlx::query("UPDATE channel_videos SET is_deleted = 1 WHERE video_id = 'vB'")
        .execute(&app.pool)
        .await
        .unwrap();

    let res = app.server.get("/api/channels/UCmeta").await;
    assert_eq!(res.status_code(), StatusCode::OK);
    let body: serde_json::Value = res.json();
    assert_eq!(body["id"], "UCmeta");
    assert_eq!(body["title"], "Meta Channel");
    assert_eq!(body["description"], "A description.");
    assert_eq!(body["video_count"], 1, "tombstoned video must not count");
    // Single thumbnail entry — keyed `default`.
    assert!(body["thumbnails"]["default"]["url"]
        .as_str()
        .unwrap()
        .starts_with("https://yt3.googleusercontent.com/"));
}

/// `get_channel` for a channel that doesn't exist in
/// `channel_sync_state` returns 404 (after passing the access gate).
#[tokio::test]
async fn get_channel_returns_404_when_no_sync_state_row() {
    let (app, auth) = boot_with_parent_and_child(AccountType::Child).await;
    let child_id = auth.account_id;
    let parent_id = app.parent_id.unwrap();

    // Allowlist the channel so the access gate passes, then drop the
    // header columns from `channels` (the row itself stays so the FK
    // from `allowlisted_channels` remains satisfied). The handler
    // checks `channel_title IS NULL` shape via fetch_optional — but
    // get_channel returns NotFound only when the row is fully missing,
    // so we test that path by removing the row with FKs turned off
    // on the same connection that runs the DELETE.
    allowlist_channel(&app.pool, child_id, parent_id, "UCmissing", Some("Missing")).await;
    let mut conn = app.pool.acquire().await.unwrap();
    sqlx::query("PRAGMA foreign_keys = OFF")
        .execute(&mut *conn)
        .await
        .unwrap();
    sqlx::query("DELETE FROM channels WHERE channel_id = 'UCmissing'")
        .execute(&mut *conn)
        .await
        .unwrap();
    sqlx::query("PRAGMA foreign_keys = ON")
        .execute(&mut *conn)
        .await
        .unwrap();
    drop(conn);

    let res = app.server.get("/api/channels/UCmissing").await;
    assert_eq!(res.status_code(), StatusCode::NOT_FOUND);
}

/// Unknown `sort` values should 400 with a clear error rather than
/// silently degrading to `latest` (the prior behaviour). Catches
/// frontend typos that would otherwise produce subtly-wrong ordering.
#[tokio::test]
async fn list_videos_rejects_unknown_sort_parameter() {
    let (app, auth) = boot_with_parent_and_child(AccountType::Child).await;
    let child_id = auth.account_id;
    let parent_id = app.parent_id.unwrap();

    allowlist_channel(&app.pool, child_id, parent_id, "UCsort", Some("Sortable")).await;

    let res = app
        .server
        .get("/api/channels/UCsort/videos?sort=oldest")
        .await;
    assert_eq!(res.status_code(), StatusCode::BAD_REQUEST);
    let body = res.text();
    assert!(
        body.contains("unknown sort"),
        "expected sort-validation message, got {body}"
    );
}

/// `sort=most_viewed` is explicitly accepted (validates the
/// whitelist arm of the match — without this test, a future
/// refactor could drop `most_viewed` from the whitelist without
/// failing existing tests).
#[tokio::test]
async fn list_videos_accepts_most_viewed_sort() {
    let (app, auth) = boot_with_parent_and_child(AccountType::Child).await;
    let child_id = auth.account_id;
    let parent_id = app.parent_id.unwrap();

    allowlist_channel(&app.pool, child_id, parent_id, "UCviews", Some("Viewed")).await;

    // Seed two videos with different view counts.
    seed_channel_video(
        &app.pool,
        "UCviews",
        Some("Viewed"),
        "low-views",
        Some("Low"),
        Some(1700000000),
        "backfill",
    )
    .await;
    seed_channel_video(
        &app.pool,
        "UCviews",
        Some("Viewed"),
        "high-views",
        Some("High"),
        Some(1700000001),
        "backfill",
    )
    .await;
    sqlx::query(
        "UPDATE channel_videos SET view_count = CASE video_id \
                                    WHEN 'low-views' THEN 10 \
                                    WHEN 'high-views' THEN 1000000 \
                                  END \
          WHERE channel_id = 'UCviews'",
    )
    .execute(&app.pool)
    .await
    .unwrap();

    let res = app
        .server
        .get("/api/channels/UCviews/videos?sort=most_viewed")
        .await;
    assert_eq!(res.status_code(), StatusCode::OK);
    let body: serde_json::Value = res.json();
    let items = body["items"].as_array().unwrap();
    assert_eq!(items[0]["video_id"], "high-views");
    assert_eq!(items[1]["video_id"], "low-views");
}

/// Regression: a blocked video in the middle of a full page used to
/// cause `next_page_token` to be `None` even when more rows existed.
/// The handler now applies blocked/hidden filters inline in SQL, so
/// the `LIMIT n` reliably returns `n` post-filter rows and the cursor
/// keeps advancing.
#[tokio::test]
async fn list_videos_pagination_survives_blocked_row_in_first_page() {
    let (app, auth) = boot_with_parent_and_child(AccountType::Child).await;
    let child_id = auth.account_id;
    let parent_id = app.parent_id.unwrap();

    // Allowlist the channel so the access gate passes.
    allowlist_channel(&app.pool, child_id, parent_id, "UCpaged", Some("Paged")).await;

    // Seed 60 channel_videos rows so we have at least 2 full pages
    // (PAGE_SIZE is 30). Newer i ⇒ later published_at, so v0059 is
    // first in the default `latest` ordering and v0000 is last.
    for i in 0..60 {
        let video_id = format!("vid-{i:04}");
        seed_channel_video(
            &app.pool,
            "UCpaged",
            Some("Paged"),
            &video_id,
            Some(&format!("Video {i}")),
            Some(2_000_000_000_i64 - i as i64),
            "rss",
        )
        .await;
    }

    // Block one video from the first page so the filter actually does
    // something.
    seed_blocked(&app.pool, child_id, parent_id, "vid-0010", Some("Video 10")).await;

    // First page: must emit a next_page_token even though one of the
    // 31st-priority videos was filtered.
    let res = app.server.get("/api/channels/UCpaged/videos").await;
    assert_eq!(res.status_code(), StatusCode::OK);
    let body: serde_json::Value = res.json();
    let items = body["items"].as_array().unwrap();
    assert_eq!(
        items.len(),
        30,
        "page 1 must return PAGE_SIZE items (blocked row excluded in SQL)"
    );
    let token = body["next_page_token"]
        .as_str()
        .expect("next_page_token must be present");

    // Blocked id must not appear anywhere in the page items.
    let ids: Vec<&str> = items
        .iter()
        .map(|i| i["video_id"].as_str().unwrap())
        .collect();
    assert!(!ids.contains(&"vid-0010"), "blocked video must not surface");

    // Page 2: load via the cursor, assert it has at least one item.
    // The exact item count depends on PAGE_SIZE math but it MUST be
    // non-empty given we seeded 60 videos and used 30 on page 1
    // (excluding the blocked one means 29 items consumed from the
    // ordered set; cursor advances by PAGE_SIZE=30 so we land on
    // vid-0030 onwards — which is also affected by blocked_videos so
    // page 2 should contain the remaining 30 rows including the
    // displaced vid that "should have" appeared on page 1).
    // page_token is base64 url-safe-no-pad, so no extra encoding needed.
    let res2 = app
        .server
        .get(&format!("/api/channels/UCpaged/videos?page_token={token}"))
        .await;
    assert_eq!(res2.status_code(), StatusCode::OK);
    let body2: serde_json::Value = res2.json();
    let items2 = body2["items"].as_array().unwrap();
    assert!(
        !items2.is_empty(),
        "page 2 must be non-empty when more rows exist"
    );
}
