//! Coverage of the usage-limit middleware.
//!
//! The middleware is applied to `/child/video/:video_id` and
//! `/api/videos/*` / `/api/proxy/*`. We hit the page handler, which
//! routes through both usage-limit and the (failing) yt-dlp extraction
//! gracefully. With a generous limit and an allowlisted-then-blocked
//! video, the middleware lets the request through to the page; with a
//! used-up limit, it returns 403.

mod common;

use axum::http::StatusCode;
use chrono::{Datelike, Local, Timelike};
use common::{boot_with_parent_and_child, insert_usage_limit};
use hometube::models::account::AccountType;

fn today_dow() -> i64 {
    Local::now().weekday().num_days_from_sunday() as i64
}

#[tokio::test]
async fn parent_visiting_video_page_redirects() {
    let (app, _auth) = boot_with_parent_and_child(AccountType::Parent).await;
    // Parents are bounced to /parent/home by the page handler before
    // the usage-limit middleware decides anything about them.
    let res = app.server.get("/child/video/abc").await;
    assert!(res.status_code().is_redirection());
}

#[tokio::test]
async fn child_with_no_limit_passes_middleware() {
    let (app, _auth) = boot_with_parent_and_child(AccountType::Child).await;
    // No usage_limits row → middleware short-circuits to "allow."
    // The yt-dlp extraction fails (no binary in PATH for the test
    // environment) so the handler falls back to its "unavailable"
    // template, but that's still a 200 response — meaning the
    // middleware did let the request through.
    let res = app.server.get("/child/video/abc").await;
    let s = res.status_code();
    // We accept 200 (rendered unavailable page) or 5xx (template
    // render failure). The key signal is *not* 403 from the middleware.
    assert_ne!(s, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn child_with_exhausted_limit_is_blocked() {
    let (app, auth) = boot_with_parent_and_child(AccountType::Child).await;
    let child_id = auth.account_id;

    // 1 minute / day limit, plus pre-baked usage that exhausts it.
    insert_usage_limit(
        &app.pool,
        child_id,
        today_dow(),
        60.0 / 3600.0,
        "00:00",
        "23:59",
    )
    .await;
    sqlx::query(
        "INSERT INTO usage_log (child_account_id, video_id, started_at, ended_at, duration_seconds) \
         VALUES (?, 'vid-x', unixepoch() - 30, unixepoch(), 120)",
    )
    .bind(child_id)
    .execute(&app.pool)
    .await
    .unwrap();

    let res = app.server.get("/child/video/abc").await;
    assert_eq!(res.status_code(), StatusCode::FORBIDDEN);
    let body: serde_json::Value = res.json();
    assert_eq!(body["reason"], "limit_exceeded");
}

#[tokio::test]
async fn child_outside_window_is_blocked() {
    let (app, auth) = boot_with_parent_and_child(AccountType::Child).await;
    let child_id = auth.account_id;

    // Configure the allowed window to a tiny range that's almost
    // certainly not "now". The window goes from 04:00 to 04:01 — if
    // the test happens to be running in that one minute the assertion
    // will be wrong, so we skip it.
    let now = Local::now();
    let now_minutes = now.hour() as i64 * 60 + now.minute() as i64;
    if (240..=241).contains(&now_minutes) {
        eprintln!("skipping outside-window test: it's currently 04:00-04:01");
        return;
    }
    insert_usage_limit(&app.pool, child_id, today_dow(), 10.0, "04:00", "04:01").await;

    let res = app.server.get("/child/video/abc").await;
    assert_eq!(res.status_code(), StatusCode::FORBIDDEN);
    let body: serde_json::Value = res.json();
    assert_eq!(body["reason"], "outside_window");
}
