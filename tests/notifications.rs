//! Parent-notification API + dispatcher service.
//!
//! Covers:
//! - empty list on a fresh app
//! - `services::notifications::dispatch` writing rows that surface
//!   through `GET /api/notifications`
//! - `unread-count` arithmetic before / after `mark_read`
//! - 404 when marking somebody else's notification read

mod common;

use axum::http::StatusCode;
use common::boot_with_parent_and_child;
use hometube::models::account::AccountType;
use hometube::services::notifications::{
    self, TYPE_NEW_SEARCH_TERM, TYPE_SYSTEM_UPDATE, TYPE_TIME_LIMIT_REACHED, TYPE_TOKEN_EXPIRED,
};

#[tokio::test]
async fn empty_initially() {
    let (app, _auth) = boot_with_parent_and_child(AccountType::Parent).await;
    let res = app.server.get("/api/notifications").await;
    assert_eq!(res.status_code(), StatusCode::OK);
    let body: serde_json::Value = res.json();
    assert!(body.as_array().unwrap().is_empty());

    let res = app.server.get("/api/notifications/unread-count").await;
    assert_eq!(res.status_code(), StatusCode::OK);
    let body: serde_json::Value = res.json();
    assert_eq!(body["unread"], 0);
}

#[tokio::test]
async fn dispatched_notification_appears_with_unread_count() {
    let (app, auth) = boot_with_parent_and_child(AccountType::Parent).await;
    let parent_id = auth.account_id;

    notifications::dispatch(
        &app.pool,
        parent_id,
        TYPE_SYSTEM_UPDATE,
        "Hello",
        "test message",
        &serde_json::json!({"context": "test"}),
    )
    .await
    .unwrap();

    let res = app.server.get("/api/notifications").await;
    let body: serde_json::Value = res.json();
    let arr = body.as_array().unwrap();
    assert_eq!(arr.len(), 1);
    assert_eq!(arr[0]["title"], "Hello");
    assert_eq!(arr[0]["notification_type"], TYPE_SYSTEM_UPDATE);
    assert_eq!(arr[0]["is_read"], 0);

    let res = app.server.get("/api/notifications/unread-count").await;
    let body: serde_json::Value = res.json();
    assert_eq!(body["unread"], 1);
}

#[tokio::test]
async fn mark_read_decrements_unread_count() {
    let (app, auth) = boot_with_parent_and_child(AccountType::Parent).await;

    notifications::dispatch(
        &app.pool,
        auth.account_id,
        TYPE_TIME_LIMIT_REACHED,
        "Limit",
        "limit reached",
        &serde_json::json!({"child_account_id": 1}),
    )
    .await
    .unwrap();

    // Find the notification ID via the list endpoint.
    let res = app.server.get("/api/notifications").await;
    let body: serde_json::Value = res.json();
    let id = body[0]["id"].as_i64().unwrap();

    let res = app
        .server
        .put(&format!("/api/notifications/{id}/read"))
        .await;
    assert_eq!(res.status_code(), StatusCode::NO_CONTENT);

    // unread-count is now zero.
    let res = app.server.get("/api/notifications/unread-count").await;
    let body: serde_json::Value = res.json();
    assert_eq!(body["unread"], 0);

    // is_read is set on the row.
    let res = app.server.get("/api/notifications").await;
    let body: serde_json::Value = res.json();
    assert_eq!(body[0]["is_read"], 1);
}

#[tokio::test]
async fn mark_read_404_for_other_parents_row() {
    let (app, _auth) = boot_with_parent_and_child(AccountType::Parent).await;
    // Insert a real second parent so the foreign-key passes, then
    // attach the notification to *that* parent. The active session
    // (the harness's first parent) should not be able to mark it read.
    let other_parent_id = common::insert_account(
        &app.pool,
        "google-other-parent",
        "other@example.test",
        "Other Parent",
        AccountType::Parent,
    )
    .await;
    sqlx::query(
        "INSERT INTO parent_notifications (parent_account_id, notification_type, title, message) \
         VALUES (?, ?, 'x', 'y')",
    )
    .bind(other_parent_id)
    .bind(TYPE_SYSTEM_UPDATE)
    .execute(&app.pool)
    .await
    .unwrap();

    let id: i64 =
        sqlx::query_scalar("SELECT id FROM parent_notifications WHERE parent_account_id = ?")
            .bind(other_parent_id)
            .fetch_one(&app.pool)
            .await
            .unwrap();

    let res = app
        .server
        .put(&format!("/api/notifications/{id}/read"))
        .await;
    assert_eq!(res.status_code(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn read_all_marks_everything_read() {
    let (app, auth) = boot_with_parent_and_child(AccountType::Parent).await;
    for i in 0..3 {
        notifications::dispatch(
            &app.pool,
            auth.account_id,
            TYPE_SYSTEM_UPDATE,
            &format!("title {i}"),
            "msg",
            &serde_json::json!({}),
        )
        .await
        .unwrap();
    }
    let res = app.server.put("/api/notifications/read-all").await;
    assert_eq!(res.status_code(), StatusCode::NO_CONTENT);

    let res = app.server.get("/api/notifications/unread-count").await;
    let body: serde_json::Value = res.json();
    assert_eq!(body["unread"], 0);
}

#[tokio::test]
async fn dispatch_token_expired_writes_one_row_per_parent() {
    let (app, _auth) = boot_with_parent_and_child(AccountType::Parent).await;
    let parent_id = app.parent_id.unwrap();

    notifications::dispatch_token_expired(
        &app.pool,
        parent_id,
        "parent@example.test",
        "Parent One",
    )
    .await
    .unwrap();

    let count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM parent_notifications WHERE notification_type = ?")
            .bind(TYPE_TOKEN_EXPIRED)
            .fetch_one(&app.pool)
            .await
            .unwrap();
    assert_eq!(count, 1);

    // Second call with the same account should be deduped within the
    // 24-hour window.
    notifications::dispatch_token_expired(
        &app.pool,
        parent_id,
        "parent@example.test",
        "Parent One",
    )
    .await
    .unwrap();
    let count_again: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM parent_notifications WHERE notification_type = ?")
            .bind(TYPE_TOKEN_EXPIRED)
            .fetch_one(&app.pool)
            .await
            .unwrap();
    assert_eq!(count_again, 1, "second call should dedupe");
}

#[tokio::test]
async fn dispatch_new_search_term_dedupes_per_query() {
    let (app, _auth) = boot_with_parent_and_child(AccountType::Parent).await;
    let child_id = app.child_id.unwrap();

    notifications::dispatch_new_search_term(&app.pool, child_id, "Tot", "dinosaurs")
        .await
        .unwrap();
    notifications::dispatch_new_search_term(&app.pool, child_id, "Tot", "dinosaurs")
        .await
        .unwrap();
    notifications::dispatch_new_search_term(&app.pool, child_id, "Tot", "trains")
        .await
        .unwrap();

    let count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM parent_notifications WHERE notification_type = ?")
            .bind(TYPE_NEW_SEARCH_TERM)
            .fetch_one(&app.pool)
            .await
            .unwrap();
    // Two distinct queries → two notifications; the duplicate "dinosaurs" is deduped.
    assert_eq!(count, 2);
}

#[tokio::test]
async fn dispatch_ytdlp_upgraded_emits_system_update() {
    let (app, _auth) = boot_with_parent_and_child(AccountType::Parent).await;

    notifications::dispatch_ytdlp_upgraded(&app.pool, Some("2024.01.01"), "2024.02.01")
        .await
        .unwrap();

    let count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM parent_notifications \
         WHERE notification_type = ? AND metadata LIKE '%ytdlp_upgraded%'",
    )
    .bind(TYPE_SYSTEM_UPDATE)
    .fetch_one(&app.pool)
    .await
    .unwrap();
    assert_eq!(count, 1);

    // Repeat with the same versions → deduped.
    notifications::dispatch_ytdlp_upgraded(&app.pool, Some("2024.01.01"), "2024.02.01")
        .await
        .unwrap();
    let count_again: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM parent_notifications \
         WHERE notification_type = ? AND metadata LIKE '%ytdlp_upgraded%'",
    )
    .bind(TYPE_SYSTEM_UPDATE)
    .fetch_one(&app.pool)
    .await
    .unwrap();
    assert_eq!(count_again, 1);

    // Different new version → fires again.
    notifications::dispatch_ytdlp_upgraded(&app.pool, Some("2024.02.01"), "2024.03.01")
        .await
        .unwrap();
    let count_three: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM parent_notifications \
         WHERE notification_type = ? AND metadata LIKE '%ytdlp_upgraded%'",
    )
    .bind(TYPE_SYSTEM_UPDATE)
    .fetch_one(&app.pool)
    .await
    .unwrap();
    assert_eq!(count_three, 2);
}

#[tokio::test]
async fn child_search_emits_new_search_term_first_time() {
    let (app, _auth) = common::boot_with_parent_and_child(AccountType::Child).await;

    let res = app.server.get("/api/search?q=dinosaurs").await;
    assert_eq!(res.status_code(), StatusCode::OK);

    let count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM parent_notifications WHERE notification_type = ?")
            .bind(TYPE_NEW_SEARCH_TERM)
            .fetch_one(&app.pool)
            .await
            .unwrap();
    assert_eq!(count, 1);

    // Same child re-searching the same term should not fire again.
    let res = app.server.get("/api/search?q=dinosaurs").await;
    assert_eq!(res.status_code(), StatusCode::OK);
    let count_again: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM parent_notifications WHERE notification_type = ?")
            .bind(TYPE_NEW_SEARCH_TERM)
            .fetch_one(&app.pool)
            .await
            .unwrap();
    assert_eq!(count_again, 1);
}

#[tokio::test]
async fn delete_notification_removes_it() {
    let (app, auth) = boot_with_parent_and_child(AccountType::Parent).await;
    notifications::dispatch(
        &app.pool,
        auth.account_id,
        TYPE_SYSTEM_UPDATE,
        "to-delete",
        "x",
        &serde_json::json!({}),
    )
    .await
    .unwrap();
    let res = app.server.get("/api/notifications").await;
    let body: serde_json::Value = res.json();
    let id = body[0]["id"].as_i64().unwrap();

    let res = app.server.delete(&format!("/api/notifications/{id}")).await;
    assert_eq!(res.status_code(), StatusCode::NO_CONTENT);

    let res = app.server.get("/api/notifications").await;
    let body: serde_json::Value = res.json();
    assert!(body.as_array().unwrap().is_empty());
}
