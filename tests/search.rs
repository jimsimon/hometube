//! Child-side allowlist-bounded search.
//!
//! `parent_search` hits the YouTube Data API directly so we don't
//! exercise it. `child_search` queries our own tables (allowlist,
//! subscriptions, watch history), so we can drive it against a small
//! fixture and assert the results / `search_log` row.

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

    common::allowlist_channel(
        &app.pool,
        child_id,
        parent_id,
        "chan-1",
        Some("Cooking with Kids"),
    )
    .await;
    common::allowlist_video(
        &app.pool,
        child_id,
        parent_id,
        "vid-1",
        Some("Cooking 101"),
        None,
    )
    .await;

    let res = app.server.get("/api/search?q=Cooking&type=all").await;
    assert_eq!(res.status_code(), StatusCode::OK);
    let body: serde_json::Value = res.json();
    assert_eq!(body["q"], "Cooking");
    let chans = body["results"]["channels"].as_array().unwrap();
    assert!(!chans.is_empty());
    let videos = body["results"]["videos"].as_array().unwrap();
    assert!(!videos.is_empty());

    // search_log gets a row.
    let count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM search_log WHERE child_account_id = ?")
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

    common::allowlist_video(
        &app.pool,
        child_id,
        parent_id,
        "vid-1",
        Some("Hello World"),
        None,
    )
    .await;

    let res = app.server.get("/api/search?q=Hello&type=video").await;
    assert_eq!(res.status_code(), StatusCode::OK);
    let body: serde_json::Value = res.json();
    assert!(body["results"]["channels"].as_array().unwrap().is_empty());
    assert!(!body["results"]["videos"].as_array().unwrap().is_empty());
}

#[tokio::test]
async fn child_search_returns_empty_for_no_match() {
    let (app, _auth) = boot_with_parent_and_child(AccountType::Child).await;
    let res = app.server.get("/api/search?q=zzznonexistent").await;
    assert_eq!(res.status_code(), StatusCode::OK);
    let body: serde_json::Value = res.json();
    assert!(body["results"]["channels"].as_array().unwrap().is_empty());
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
async fn child_search_lists_a_channel_once_when_allowlisted_and_subscribed() {
    // Regression: a channel that is both allowlisted and subscribed used
    // to appear twice whenever the subscription's denormalised
    // title/thumbnail copy drifted from the canonical `channels` row.
    let (app, auth) = boot_with_parent_and_child(AccountType::Child).await;
    let child_id = auth.account_id;
    let parent_id = app.parent_id.unwrap();

    common::allowlist_channel(
        &app.pool,
        child_id,
        parent_id,
        "chan-dup",
        Some("Dino Facts"),
    )
    .await;
    sqlx::query(
        "UPDATE channels SET channel_thumbnail_url = 'https://yt3.example/new.jpg' \
         WHERE channel_id = 'chan-dup'",
    )
    .execute(&app.pool)
    .await
    .unwrap();
    // Subscription captured an older avatar URL for the same channel.
    sqlx::query(
        "INSERT INTO child_subscriptions \
            (child_account_id, channel_id, channel_title, channel_thumbnail_url) \
         VALUES (?, 'chan-dup', 'Dino Facts', 'https://yt3.example/old.jpg')",
    )
    .bind(child_id)
    .execute(&app.pool)
    .await
    .unwrap();

    let res = app.server.get("/api/search?q=Dino&type=channel").await;
    assert_eq!(res.status_code(), StatusCode::OK);
    let body: serde_json::Value = res.json();
    let chans = body["results"]["channels"].as_array().unwrap();
    assert_eq!(chans.len(), 1, "expected one channel hit, got {chans:?}");
    assert_eq!(chans[0]["channel_id"], "chan-dup");
    // The canonical `channels` metadata wins over the subscription copy.
    assert_eq!(
        chans[0]["channel_thumbnail_url"],
        "https://yt3.example/new.jpg"
    );
}

#[tokio::test]
async fn child_search_still_finds_a_subscription_only_channel() {
    // The GROUP BY must not drop channels that exist only in
    // `child_subscriptions` (no allowlist row).
    let (app, auth) = boot_with_parent_and_child(AccountType::Child).await;
    let child_id = auth.account_id;
    sqlx::query(
        "INSERT INTO child_subscriptions (child_account_id, channel_id, channel_title) \
         VALUES (?, 'chan-sub-only', 'Space Songs')",
    )
    .bind(child_id)
    .execute(&app.pool)
    .await
    .unwrap();

    let res = app.server.get("/api/search?q=Space&type=channel").await;
    assert_eq!(res.status_code(), StatusCode::OK);
    let body: serde_json::Value = res.json();
    let chans = body["results"]["channels"].as_array().unwrap();
    assert_eq!(chans.len(), 1);
    assert_eq!(chans[0]["channel_title"], "Space Songs");
}
