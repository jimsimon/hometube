//! Integration tests for the YouTube outbound-sync service.
//!
//! These tests exercise the public `push_*_change` functions by seeding
//! DB rows in the `pending_*` state and calling the sync function. Since
//! the test harness uses fake OAuth tokens, the actual YouTube API call
//! will fail — this tests the error-handling path: rows should be marked
//! `error` after retries are exhausted.
//!
//! We also test the no-op paths (already synced, missing rows).
//!
//! **Performance note:** The `_marks_error_` tests take ~5s each because
//! `retry_push` in `sync.rs` uses real exponential backoff (1s + 4s)
//! before marking a row as `error`. This is intentional — we're testing
//! the actual retry + error-marking flow end-to-end. Total file runtime
//! is ~35s.

mod common;

use common::{boot, insert_account, seed_credentials};
use hometube::models::account::AccountType;
use hometube::services::setup::{set_config_value, KEY_SETUP_COMPLETE};
use hometube::services::sync;

/// Helper: seed a child account with valid (but fake) OAuth tokens.
async fn setup_child(pool: &sqlx::SqlitePool) -> i64 {
    seed_credentials(pool).await;
    set_config_value(pool, KEY_SETUP_COMPLETE, "true")
        .await
        .unwrap();
    insert_account(
        pool,
        "google-parent-1",
        "parent@example.test",
        "Parent",
        AccountType::Parent,
    )
    .await;
    insert_account(
        pool,
        "google-child-1",
        "child@example.test",
        "Child",
        AccountType::Child,
    )
    .await
}

// ---------------------------------------------------------------------------
// push_subscription_change
// ---------------------------------------------------------------------------

#[tokio::test]
async fn push_subscription_noop_when_no_row() {
    let app = boot().await;
    let child_id = setup_child(&app.pool).await;

    // No subscription row exists — should be a no-op.
    let result = sync::push_subscription_change(&app.pool, child_id, "UC_nonexistent").await;
    assert!(result.is_ok());
}

#[tokio::test]
async fn push_subscription_noop_when_already_synced() {
    let app = boot().await;
    let child_id = setup_child(&app.pool).await;

    sqlx::query(
        "INSERT INTO child_subscriptions \
         (child_account_id, channel_id, channel_title, sync_status, is_deleted, source) \
         VALUES (?, 'UC_test', 'Test', 'synced', 0, 'app')",
    )
    .bind(child_id)
    .execute(&app.pool)
    .await
    .unwrap();

    let result = sync::push_subscription_change(&app.pool, child_id, "UC_test").await;
    assert!(result.is_ok());

    // Status should remain unchanged.
    let status: String = sqlx::query_scalar(
        "SELECT sync_status FROM child_subscriptions \
         WHERE child_account_id = ? AND channel_id = 'UC_test'",
    )
    .bind(child_id)
    .fetch_one(&app.pool)
    .await
    .unwrap();
    assert_eq!(status, "synced");
}

#[tokio::test]
async fn push_subscription_marks_error_on_api_failure() {
    let app = boot().await;
    let child_id = setup_child(&app.pool).await;

    // Seed a pending_push subscription.
    sqlx::query(
        "INSERT INTO child_subscriptions \
         (child_account_id, channel_id, channel_title, sync_status, is_deleted, source) \
         VALUES (?, 'UC_fail', 'Fail Channel', 'pending_push', 0, 'app')",
    )
    .bind(child_id)
    .execute(&app.pool)
    .await
    .unwrap();

    // This will fail because the token is fake and YouTube won't accept it.
    // The function should mark the row as 'error' rather than panicking.
    let result = sync::push_subscription_change(&app.pool, child_id, "UC_fail").await;
    assert!(result.is_ok()); // The function is total — doesn't return Err.

    let status: String = sqlx::query_scalar(
        "SELECT sync_status FROM child_subscriptions \
         WHERE child_account_id = ? AND channel_id = 'UC_fail'",
    )
    .bind(child_id)
    .fetch_one(&app.pool)
    .await
    .unwrap();
    assert_eq!(status, "error");
}

#[tokio::test]
async fn push_subscription_delete_noop_without_youtube_id() {
    let app = boot().await;
    let child_id = setup_child(&app.pool).await;

    // pending_delete but no youtube_subscription_id → collapses to local-only synced.
    sqlx::query(
        "INSERT INTO child_subscriptions \
         (child_account_id, channel_id, channel_title, sync_status, is_deleted, source, youtube_subscription_id) \
         VALUES (?, 'UC_del', 'Del', 'pending_delete', 1, 'app', NULL)",
    )
    .bind(child_id)
    .execute(&app.pool)
    .await
    .unwrap();

    let result = sync::push_subscription_change(&app.pool, child_id, "UC_del").await;
    assert!(result.is_ok());

    let status: String = sqlx::query_scalar(
        "SELECT sync_status FROM child_subscriptions \
         WHERE child_account_id = ? AND channel_id = 'UC_del'",
    )
    .bind(child_id)
    .fetch_one(&app.pool)
    .await
    .unwrap();
    assert_eq!(status, "synced");
}

// ---------------------------------------------------------------------------
// push_like_change
// ---------------------------------------------------------------------------

#[tokio::test]
async fn push_like_noop_when_no_row() {
    let app = boot().await;
    let child_id = setup_child(&app.pool).await;

    let result = sync::push_like_change(&app.pool, child_id, "no_such_video").await;
    assert!(result.is_ok());
}

#[tokio::test]
async fn push_like_noop_when_already_synced() {
    let app = boot().await;
    let child_id = setup_child(&app.pool).await;

    sqlx::query(
        "INSERT INTO video_likes \
         (child_account_id, video_id, sync_status, is_deleted, source) \
         VALUES (?, 'vid1', 'synced', 0, 'app')",
    )
    .bind(child_id)
    .execute(&app.pool)
    .await
    .unwrap();

    let result = sync::push_like_change(&app.pool, child_id, "vid1").await;
    assert!(result.is_ok());

    let status: String = sqlx::query_scalar(
        "SELECT sync_status FROM video_likes \
         WHERE child_account_id = ? AND video_id = 'vid1'",
    )
    .bind(child_id)
    .fetch_one(&app.pool)
    .await
    .unwrap();
    assert_eq!(status, "synced");
}

#[tokio::test]
async fn push_like_marks_error_on_api_failure() {
    let app = boot().await;
    let child_id = setup_child(&app.pool).await;

    sqlx::query(
        "INSERT INTO video_likes \
         (child_account_id, video_id, sync_status, is_deleted, source) \
         VALUES (?, 'vid_fail', 'pending_push', 0, 'app')",
    )
    .bind(child_id)
    .execute(&app.pool)
    .await
    .unwrap();

    let result = sync::push_like_change(&app.pool, child_id, "vid_fail").await;
    assert!(result.is_ok());

    let status: String = sqlx::query_scalar(
        "SELECT sync_status FROM video_likes \
         WHERE child_account_id = ? AND video_id = 'vid_fail'",
    )
    .bind(child_id)
    .fetch_one(&app.pool)
    .await
    .unwrap();
    assert_eq!(status, "error");
}

// ---------------------------------------------------------------------------
// push_playlist_change
// ---------------------------------------------------------------------------

#[tokio::test]
async fn push_playlist_noop_when_not_own() {
    let app = boot().await;
    let child_id = setup_child(&app.pool).await;

    // is_own=0 → read-only import, should be a no-op regardless of status.
    sqlx::query(
        "INSERT INTO child_playlists \
         (child_account_id, title, sync_status, is_deleted, is_own, source) \
         VALUES (?, 'Imported PL', 'pending_create', 0, 0, 'youtube')",
    )
    .bind(child_id)
    .execute(&app.pool)
    .await
    .expect("insert imported playlist");

    let pl_id: i64 = sqlx::query_scalar(
        "SELECT id FROM child_playlists WHERE child_account_id = ? AND title = 'Imported PL'",
    )
    .bind(child_id)
    .fetch_one(&app.pool)
    .await
    .unwrap();

    let result = sync::push_playlist_change(&app.pool, child_id, pl_id).await;
    assert!(result.is_ok());

    // Status unchanged.
    let status: String = sqlx::query_scalar("SELECT sync_status FROM child_playlists WHERE id = ?")
        .bind(pl_id)
        .fetch_one(&app.pool)
        .await
        .unwrap();
    assert_eq!(status, "pending_create");
}

#[tokio::test]
async fn push_playlist_create_marks_error_on_api_failure() {
    let app = boot().await;
    let child_id = setup_child(&app.pool).await;

    sqlx::query(
        "INSERT INTO child_playlists \
         (child_account_id, title, sync_status, is_deleted, is_own, source) \
         VALUES (?, 'My Playlist', 'pending_create', 0, 1, 'app')",
    )
    .bind(child_id)
    .execute(&app.pool)
    .await
    .unwrap();

    let pl_id: i64 = sqlx::query_scalar(
        "SELECT id FROM child_playlists WHERE child_account_id = ? AND title = 'My Playlist'",
    )
    .bind(child_id)
    .fetch_one(&app.pool)
    .await
    .unwrap();

    let result = sync::push_playlist_change(&app.pool, child_id, pl_id).await;
    assert!(result.is_ok());

    let status: String = sqlx::query_scalar("SELECT sync_status FROM child_playlists WHERE id = ?")
        .bind(pl_id)
        .fetch_one(&app.pool)
        .await
        .unwrap();
    assert_eq!(status, "error");
}

#[tokio::test]
async fn push_playlist_delete_without_yt_id_syncs_locally() {
    let app = boot().await;
    let child_id = setup_child(&app.pool).await;

    sqlx::query(
        "INSERT INTO child_playlists \
         (child_account_id, title, sync_status, is_deleted, is_own, source, youtube_playlist_id) \
         VALUES (?, 'Del PL', 'pending_delete', 1, 1, 'app', NULL)",
    )
    .bind(child_id)
    .execute(&app.pool)
    .await
    .unwrap();

    let pl_id: i64 = sqlx::query_scalar(
        "SELECT id FROM child_playlists WHERE child_account_id = ? AND title = 'Del PL'",
    )
    .bind(child_id)
    .fetch_one(&app.pool)
    .await
    .unwrap();

    let result = sync::push_playlist_change(&app.pool, child_id, pl_id).await;
    assert!(result.is_ok());

    let status: String = sqlx::query_scalar("SELECT sync_status FROM child_playlists WHERE id = ?")
        .bind(pl_id)
        .fetch_one(&app.pool)
        .await
        .unwrap();
    assert_eq!(status, "synced");
}

// ---------------------------------------------------------------------------
// sync_youtube_for_all_children
// ---------------------------------------------------------------------------

#[tokio::test]
async fn sync_all_children_handles_no_children_gracefully() {
    let app = boot().await;
    seed_credentials(&app.pool).await;
    set_config_value(&app.pool, KEY_SETUP_COMPLETE, "true")
        .await
        .unwrap();
    // Only a parent — no children.
    insert_account(
        &app.pool,
        "google-parent-1",
        "parent@example.test",
        "Parent",
        AccountType::Parent,
    )
    .await;

    let result = sync::sync_youtube_for_all_children(&app.pool).await;
    assert!(result.is_ok());
    assert!(result.unwrap().contains("0")); // 0 children processed
}
