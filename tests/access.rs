//! Direct tests for [`hometube::services::access::can_child_view`].
//!
//! Each scenario builds a small fixture (one child + a mix of allow /
//! block table rows) and asserts the access decision matches the
//! documented precedence:
//!
//! 1. Blocked overrides everything → deny.
//! 2. Direct video allowlist → allow.
//! 3. Channel allowlist → allow.
//! 4. None of the above → deny.

mod common;

use common::{allowlist_channel, allowlist_video, boot, insert_account, seed_video};
use hometube::models::account::AccountType;
use hometube::services::access::can_child_view;

#[tokio::test]
async fn allowlisted_video_is_allowed() {
    let app = boot().await;
    let child_id = insert_account(&app.pool, "C", AccountType::Child).await;
    let parent_id = insert_account(&app.pool, "P", AccountType::Parent).await;

    allowlist_video(
        &app.pool,
        child_id,
        parent_id,
        "vid-allow",
        Some("allow"),
        None,
    )
    .await;

    let allowed = can_child_view(&app.pool, child_id, "vid-allow", None)
        .await
        .unwrap();
    assert!(allowed);
}

#[tokio::test]
async fn allowlisted_channel_is_allowed() {
    let app = boot().await;
    let child_id = insert_account(&app.pool, "C", AccountType::Child).await;
    let parent_id = insert_account(&app.pool, "P", AccountType::Parent).await;

    allowlist_channel(&app.pool, child_id, parent_id, "chan-1", Some("C1")).await;

    let allowed = can_child_view(&app.pool, child_id, "vid-x", Some("chan-1"))
        .await
        .unwrap();
    assert!(allowed);
}

#[tokio::test]
async fn blocked_overrides_allowlist() {
    let app = boot().await;
    let child_id = insert_account(&app.pool, "C", AccountType::Child).await;
    let parent_id = insert_account(&app.pool, "P", AccountType::Parent).await;

    // Allowlisted via direct video AND channel...
    allowlist_video(
        &app.pool,
        child_id,
        parent_id,
        "vid-block",
        Some("t"),
        Some("chan-1"),
    )
    .await;
    allowlist_channel(&app.pool, child_id, parent_id, "chan-1", Some("C")).await;

    // ...but explicitly blocked. `videos` row already seeded above; just
    // add the per-child block.
    seed_video(&app.pool, "vid-block", Some("t"), Some("chan-1")).await;
    sqlx::query(
        "INSERT INTO blocked_videos (child_account_id, video_id, blocked_by) \
         VALUES (?, 'vid-block', ?)",
    )
    .bind(child_id)
    .bind(parent_id)
    .execute(&app.pool)
    .await
    .unwrap();

    let allowed = can_child_view(&app.pool, child_id, "vid-block", Some("chan-1"))
        .await
        .unwrap();
    assert!(
        !allowed,
        "blocked video must be denied even when allowlisted"
    );
}

#[tokio::test]
async fn unrelated_video_is_denied() {
    let app = boot().await;
    let child_id = insert_account(&app.pool, "C", AccountType::Child).await;

    let allowed = can_child_view(&app.pool, child_id, "vid-unknown", Some("chan-other"))
        .await
        .unwrap();
    assert!(!allowed);
}
