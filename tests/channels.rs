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
