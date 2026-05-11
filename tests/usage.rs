//! Usage-tracking heartbeat behaviour.
//!
//! Two scenarios:
//! 1. Anonymous heartbeat → 401 (the route is gated by `require_child`).
//! 2. Child heartbeat → upserts `usage_log` and `watch_history`. After
//!    enough heartbeats accumulate to exhaust the daily limit, the
//!    response carries `limit_exceeded: true` and a row appears in
//!    `parent_notifications` with type `time_limit_reached`.

mod common;

use axum::http::StatusCode;
use chrono::{Datelike, Local};
use common::{boot_with_parent_and_child, insert_usage_limit};
use hometube::middleware::auth::SESSION_COOKIE;
use hometube::models::account::AccountType;
use serde_json::json;
use tower_cookies::cookie::Cookie;

fn today_dow() -> i64 {
    Local::now().weekday().num_days_from_sunday() as i64
}

#[tokio::test]
async fn anonymous_heartbeat_is_unauthorized() {
    let (app, _auth) = boot_with_parent_and_child(AccountType::Child).await;
    let bad = Cookie::new(SESSION_COOKIE, "junk");
    let res = app
        .server
        .post("/api/usage/heartbeat")
        .clear_cookies()
        .add_cookie(bad)
        .json(&json!({ "video_id": "vid-1", "position_seconds": 30 }))
        .await;
    assert_eq!(res.status_code(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn child_heartbeat_writes_usage_log_and_watch_history() {
    let (app, auth) = boot_with_parent_and_child(AccountType::Child).await;
    let child_id = auth.account_id;

    // Configure a generous limit so we don't hit limit_exceeded here.
    insert_usage_limit(&app.pool, child_id, today_dow(), 10.0, "00:00", "23:59").await;

    let res = app
        .server
        .post("/api/usage/heartbeat")
        .json(&json!({
            "video_id": "vid-1",
            "position_seconds": 30,
            "duration_seconds": 600,
            "video_title": "Hello",
            "video_thumbnail_url": "http://thumb",
            "channel_title": "Some Channel",
            "elapsed_seconds": 30,
        }))
        .await;
    assert_eq!(res.status_code(), StatusCode::OK);
    let body: serde_json::Value = res.json();
    assert_eq!(body["limit_exceeded"], false);
    assert!(body["remaining_seconds"].as_i64().unwrap() > 0);

    // Verify usage_log + watch_history were updated.
    let usage_count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM usage_log WHERE child_account_id = ?")
            .bind(child_id)
            .fetch_one(&app.pool)
            .await
            .unwrap();
    assert!(usage_count >= 1);

    let history_count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM watch_history WHERE child_account_id = ?")
            .bind(child_id)
            .fetch_one(&app.pool)
            .await
            .unwrap();
    assert_eq!(history_count, 1);
}

#[tokio::test]
async fn limit_exceeded_response_and_notification() {
    let (app, auth) = boot_with_parent_and_child(AccountType::Child).await;
    let child_id = auth.account_id;

    // Tiny limit (60s) for today so a single heartbeat tips us over.
    insert_usage_limit(&app.pool, child_id, today_dow(), 60.0 / 3600.0, "00:00", "23:59").await;

    // Pre-load usage_log with enough seconds to push past the cap.
    sqlx::query(
        "INSERT INTO usage_log (child_account_id, video_id, started_at, ended_at, duration_seconds) \
         VALUES (?, 'vid-1', unixepoch() - 60, unixepoch(), 120)",
    )
    .bind(child_id)
    .execute(&app.pool)
    .await
    .unwrap();

    let res = app
        .server
        .post("/api/usage/heartbeat")
        .json(&json!({
            "video_id": "vid-1",
            "position_seconds": 60,
            "elapsed_seconds": 30,
        }))
        .await;
    assert_eq!(res.status_code(), StatusCode::OK);
    let body: serde_json::Value = res.json();
    assert_eq!(body["limit_exceeded"], true);
    assert_eq!(body["reason"], "limit_exceeded");

    // A `time_limit_reached` notification was broadcast to the parent.
    let n: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM parent_notifications WHERE notification_type = 'time_limit_reached'",
    )
    .fetch_one(&app.pool)
    .await
    .unwrap();
    assert!(n >= 1, "expected a time_limit_reached notification");
}
