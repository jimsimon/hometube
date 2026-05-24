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
use common::boot_with_parent_and_child;
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

    sqlx::query(
        "INSERT INTO allowlisted_channels (child_account_id, channel_id, channel_title, added_by) \
         VALUES (?, 'chan-ok', 'Cool', ?)",
    )
    .bind(child_id)
    .bind(parent_id)
    .execute(&app.pool)
    .await
    .unwrap();

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
    sqlx::query(
        "INSERT INTO allowlisted_channels (child_account_id, channel_id, channel_title, added_by) \
         VALUES (?, 'UCpaged', 'Paged', ?)",
    )
    .bind(child_id)
    .bind(parent_id)
    .execute(&app.pool)
    .await
    .unwrap();

    // Seed 60 channel_videos rows so we have at least 2 full pages
    // (PAGE_SIZE is 30).
    for i in 0..60 {
        let video_id = format!("vid-{i:04}");
        sqlx::query(
            "INSERT INTO channel_videos \
                (channel_id, video_id, title, channel_title, published_at, \
                 first_seen_at, last_seen_at, source) \
             VALUES ('UCpaged', ?, ?, 'Paged', ?, 1, 1, 'rss')",
        )
        .bind(&video_id)
        // Newer i ⇒ later published_at, so v0059 is first in the
        // default `latest` ordering and v0000 is last.
        .bind(format!("Video {i}"))
        .bind(2_000_000_000_i64 - i as i64)
        .execute(&app.pool)
        .await
        .unwrap();
    }

    // Block one video from the first page so the filter actually does
    // something.
    sqlx::query(
        "INSERT INTO blocked_videos (child_account_id, video_id, blocked_by) \
         VALUES (?, 'vid-0010', ?)",
    )
    .bind(child_id)
    .bind(parent_id)
    .execute(&app.pool)
    .await
    .unwrap();

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
