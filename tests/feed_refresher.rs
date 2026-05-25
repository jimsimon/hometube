//! Integration test: the RSS poll path writes into `channel_videos`
//! and the `/api/feed/new-videos` handler then serves those rows.
//!
//! We bypass the long-running [`feed_refresher`] loop and call
//! [`youtube_rss::poll_channel`] directly (which is what the loop does
//! per source). The point here is to validate the full
//! HTTP-mock → parser → DB → handler round-trip.

mod common;

use std::time::Duration;

use axum::http::StatusCode;
use common::boot_with_parent_and_child;
use hometube::models::account::AccountType;
use hometube::services::feed_cache;
use hometube::services::feed_refresher::{
    self, KEY_CHANNEL_INTERVAL_S, KEY_DISPATCH_DELAY_MS, KEY_SIDECAR_FALLBACK_ENABLED,
    KEY_SIDECAR_FALLBACK_MAX_PER_HOUR, KEY_SIDECAR_FALLBACK_MIN_INTERVAL_S,
};
use hometube::services::setup::set_config_value;
use hometube::services::youtube_rss::{self, PollOutcome, KEY_RSS_BASE_URL};
use serde_json::json;
use wiremock::matchers::{method, path, path_regex, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

const SAMPLE_FEED: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<feed xmlns:yt="http://www.youtube.com/xml/schemas/2015"
      xmlns:media="http://search.yahoo.com/mrss/"
      xmlns="http://www.w3.org/2005/Atom">
  <title>Test Channel</title>
  <entry>
    <yt:videoId>vid-rss-1</yt:videoId>
    <yt:channelId>UCtest</yt:channelId>
    <title>RSS Episode 1</title>
    <author><name>Test Channel</name></author>
    <published>2024-06-15T10:00:00+00:00</published>
    <media:group>
      <media:thumbnail url="https://i.ytimg.com/vi/vid-rss-1/hqdefault.jpg"/>
    </media:group>
  </entry>
  <entry>
    <yt:videoId>vid-rss-2</yt:videoId>
    <yt:channelId>UCtest</yt:channelId>
    <title>RSS Episode 2</title>
    <author><name>Test Channel</name></author>
    <published>2024-06-16T10:00:00+00:00</published>
  </entry>
</feed>"#;

#[tokio::test]
async fn rss_poll_populates_feed_then_handler_serves_it() {
    let mock_server = MockServer::start().await;
    let (app, auth) = boot_with_parent_and_child(AccountType::Child).await;
    let child_id = auth.account_id;
    let parent_id = app.parent_id.unwrap();

    set_config_value(&app.pool, KEY_RSS_BASE_URL, &mock_server.uri())
        .await
        .unwrap();

    // Allowlist the channel + register as a feed source.
    sqlx::query(
        "INSERT INTO allowlisted_channels \
            (child_account_id, channel_id, channel_title, added_by) \
         VALUES (?, 'UCtest', 'Test Channel', ?)",
    )
    .bind(child_id)
    .bind(parent_id)
    .execute(&app.pool)
    .await
    .unwrap();
    feed_cache::upsert_channel(&app.pool, "UCtest")
        .await
        .unwrap();

    Mock::given(method("GET"))
        .and(path("/feeds/videos.xml"))
        .and(query_param("channel_id", "UCtest"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("etag", "\"abc\"")
                .set_body_raw(SAMPLE_FEED.as_bytes().to_vec(), "application/atom+xml"),
        )
        .mount(&mock_server)
        .await;

    // Drive one poll synchronously, then persist via feed_cache (this
    // is what `feed_refresher::poll_one` does under the hood).
    let http = reqwest::Client::new();
    let outcome = youtube_rss::poll_channel(&http, &mock_server.uri(), "UCtest", None, None)
        .await
        .unwrap();
    match outcome {
        PollOutcome::Updated { items, etag, .. } => {
            assert_eq!(items.len(), 2);
            assert_eq!(etag.as_deref(), Some("\"abc\""));
            feed_cache::upsert_channel_videos_from_rss(&app.pool, "UCtest", &items, 1_718_531_999)
                .await
                .unwrap();
        }
        PollOutcome::NotModified => panic!("expected Updated"),
    }

    // Now hit the handler.
    let res = app.server.get("/api/feed/new-videos").await;
    assert_eq!(res.status_code(), StatusCode::OK);
    let body: serde_json::Value = res.json();
    let arr = body.as_array().unwrap();
    assert_eq!(arr.len(), 2);
    // Most recent first.
    assert_eq!(arr[0]["video_id"], "vid-rss-2");
    assert_eq!(arr[0]["title"], "RSS Episode 2");
    assert_eq!(arr[0]["source_kind"], "channel");
    assert_eq!(arr[0]["source_id"], "UCtest");
    assert_eq!(arr[1]["video_id"], "vid-rss-1");
}

#[tokio::test]
async fn rss_304_preserves_existing_items() {
    let mock_server = MockServer::start().await;
    let (app, _auth) = boot_with_parent_and_child(AccountType::Child).await;

    set_config_value(&app.pool, KEY_RSS_BASE_URL, &mock_server.uri())
        .await
        .unwrap();
    feed_cache::upsert_channel(&app.pool, "UC304")
        .await
        .unwrap();
    feed_cache::upsert_channel_videos_from_rss(
        &app.pool,
        "UC304",
        &[feed_cache::ItemRow {
            video_id: "kept".into(),
            title: "Kept".into(),
            channel_id: Some("UC304".into()),
            channel_title: None,
            thumbnail_url: None,
            published_at: Some(1_000_000),
            published_raw: Some("1970-01-12T13:46:40Z".into()),
        }],
        1_000_000,
    )
    .await
    .unwrap();

    Mock::given(method("GET"))
        .and(path("/feeds/videos.xml"))
        .and(query_param("channel_id", "UC304"))
        .respond_with(ResponseTemplate::new(304))
        .mount(&mock_server)
        .await;

    let http = reqwest::Client::new();
    let outcome = youtube_rss::poll_channel(
        &http,
        &mock_server.uri(),
        "UC304",
        Some("\"existing-tag\""),
        None,
    )
    .await
    .unwrap();
    assert!(matches!(outcome, PollOutcome::NotModified));

    let count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM channel_videos WHERE channel_id='UC304'")
            .fetch_one(&app.pool)
            .await
            .unwrap();
    assert_eq!(count, 1, "304 must not clear cached items");
}

/// End-to-end test of the actual `feed_refresher::run` loop. Unlike
/// the earlier tests which call `poll_channel` directly, this one
/// spawns the production background task with a tight dispatch
/// interval, lets it pick up a seeded `channel_sync_state` row, and
/// asserts the row's items appear in `channel_videos` afterwards.
///
/// Covers the otherwise-untested code paths:
///   - `claim_due_sources` lease acquisition
///   - rate-gate dispatch interval
///   - bounded-inflight semaphore
///   - the loop's outer drain step
#[tokio::test]
async fn refresher_loop_polls_seeded_source_end_to_end() {
    let mock_server = MockServer::start().await;
    let (app, _auth) = boot_with_parent_and_child(AccountType::Child).await;

    // Point the refresher's RSS client + tighten its cadence so the
    // test takes seconds rather than minutes.
    set_config_value(&app.pool, KEY_RSS_BASE_URL, &mock_server.uri())
        .await
        .unwrap();
    set_config_value(&app.pool, KEY_DISPATCH_DELAY_MS, "50")
        .await
        .unwrap();
    set_config_value(&app.pool, KEY_CHANNEL_INTERVAL_S, "60")
        .await
        .unwrap();

    feed_cache::upsert_channel(&app.pool, "UCloop")
        .await
        .unwrap();

    Mock::given(method("GET"))
        .and(path("/feeds/videos.xml"))
        .and(query_param("channel_id", "UCloop"))
        .respond_with(
            ResponseTemplate::new(200).set_body_raw(
                br#"<?xml version="1.0" encoding="UTF-8"?>
<feed xmlns:yt="http://www.youtube.com/xml/schemas/2015"
      xmlns="http://www.w3.org/2005/Atom">
  <title>Loop Channel</title>
  <entry>
    <yt:videoId>loop-vid</yt:videoId>
    <yt:channelId>UCloop</yt:channelId>
    <title>Loop Episode</title>
    <author><name>Loop Channel</name></author>
    <published>2024-07-01T00:00:00+00:00</published>
  </entry>
</feed>"#
                    .to_vec(),
                "application/atom+xml",
            ),
        )
        .mount(&mock_server)
        .await;

    // Spawn the refresher and let it drive the source.
    let handle = tokio::spawn(feed_refresher::run(app.pool.clone()));

    // Poll the DB until the seeded source has the expected item, or
    // give up after ~15 s. The generous deadline absorbs slow CI
    // runners; the loop exits early as soon as the row appears so a
    // healthy run still completes in well under a second.
    // `tokio::time::pause` is not an option here because wiremock and
    // sqlx both rely on real I/O, so we deliberately use a wall-clock
    // budget with early exit instead.
    let deadline = std::time::Instant::now() + Duration::from_secs(15);
    let mut found = false;
    while std::time::Instant::now() < deadline {
        let count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM channel_videos \
             WHERE channel_id='UCloop' AND video_id='loop-vid'",
        )
        .fetch_one(&app.pool)
        .await
        .unwrap_or(0);
        if count == 1 {
            found = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    handle.abort();
    assert!(found, "refresher did not populate the seeded source");

    // The bookkeeping should reflect a successful poll: rss_last_success_at
    // set, no error, rss_consecutive_errors reset.
    let (ls, err, errs): (Option<i64>, Option<String>, i64) = sqlx::query_as(
        "SELECT rss_last_success_at, rss_last_error, rss_consecutive_errors \
           FROM channel_sync_state WHERE channel_id='UCloop'",
    )
    .fetch_one(&app.pool)
    .await
    .unwrap();
    assert!(ls.is_some(), "rss_last_success_at must be set after a poll");
    assert!(err.is_none(), "no error expected, got {err:?}");
    assert_eq!(errs, 0);
}

// ===========================================================================
// Sidecar fallback (RSS fails → sidecar steps in)
// ===========================================================================
//
// These three tests share the same end-to-end shape used above:
// wiremock the RSS host with a 404, wiremock the sidecar with a per-test
// response, spawn `feed_refresher::run`, and poll the DB until the
// expected `feed_sources` state appears.
//
// We deliberately exercise the production loop (`feed_refresher::run`)
// rather than calling `run_sidecar_fallback` directly because the
// helpers are private. Going through the loop also covers the
// rate-cap eligibility check and the reservation write that the
// helpers don't expose.

/// Common test setup for the three fallback tests below. Returns the
/// app, the RSS mock server, and the sidecar mock server. Both mocks
/// are wired into `app_config` and the refresher cadence is tightened
/// for a fast test.
async fn setup_fallback_test() -> (common::TestApp, MockServer, MockServer) {
    let rss_mock = MockServer::start().await;
    let sidecar_mock = MockServer::start().await;
    let (app, _auth) = boot_with_parent_and_child(AccountType::Child).await;

    set_config_value(&app.pool, KEY_RSS_BASE_URL, &rss_mock.uri())
        .await
        .unwrap();
    set_config_value(&app.pool, "discovery_sidecar_url", &sidecar_mock.uri())
        .await
        .unwrap();
    // Tighten cadence so the test resolves quickly.
    set_config_value(&app.pool, KEY_DISPATCH_DELAY_MS, "50")
        .await
        .unwrap();
    set_config_value(&app.pool, KEY_CHANNEL_INTERVAL_S, "60")
        .await
        .unwrap();
    // Permit fallbacks: enabled + per-source cap of 1 minute (so the
    // initial NULL `last_sidecar_fallback_at` permits a fallback
    // immediately).
    set_config_value(&app.pool, KEY_SIDECAR_FALLBACK_ENABLED, "true")
        .await
        .unwrap();
    set_config_value(&app.pool, KEY_SIDECAR_FALLBACK_MIN_INTERVAL_S, "60")
        .await
        .unwrap();
    set_config_value(&app.pool, KEY_SIDECAR_FALLBACK_MAX_PER_HOUR, "120")
        .await
        .unwrap();

    (app, rss_mock, sidecar_mock)
}

/// Poll the DB until `predicate` returns true, or fail after ~10 s.
/// Mirrors the deadline pattern used by `refresher_loop_polls_seeded_source_end_to_end`.
async fn wait_until<F>(label: &str, mut predicate: F)
where
    F: FnMut() -> std::pin::Pin<Box<dyn std::future::Future<Output = bool> + Send>>,
{
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while std::time::Instant::now() < deadline {
        if predicate().await {
            return;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("timed out waiting for: {label}");
}

#[tokio::test]
async fn fallback_writes_items_when_rss_fails_and_sidecar_returns_items() {
    let (app, rss_mock, sidecar_mock) = setup_fallback_test().await;

    feed_cache::upsert_channel(&app.pool, "UCfb1")
        .await
        .unwrap();

    // RSS returns 404 (simulates the YouTube outage).
    Mock::given(method("GET"))
        .and(path("/feeds/videos.xml"))
        .and(query_param("channel_id", "UCfb1"))
        .respond_with(ResponseTemplate::new(404))
        .mount(&rss_mock)
        .await;

    // Sidecar returns one item.
    Mock::given(method("GET"))
        .and(path("/channel-videos/UCfb1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "items": [
                {
                    "video_id": "vid-from-sidecar",
                    "title": "Fallback Video",
                    "channel_id": "UCfb1",
                    "channel_title": "Fallback Channel",
                    "thumbnails": {
                        "high": {"url": "https://t.test/x.jpg", "width": 480, "height": 360}
                    },
                    "published_at": "3 days ago",
                    "position": null
                }
            ],
            "next_page_token": null
        })))
        .mount(&sidecar_mock)
        .await;

    let handle = tokio::spawn(feed_refresher::run(app.pool.clone()));

    let pool = app.pool.clone();
    wait_until("fallback item appears in channel_videos", move || {
        let pool = pool.clone();
        Box::pin(async move {
            let n: i64 = sqlx::query_scalar(
                "SELECT COUNT(*) FROM channel_videos \
                 WHERE channel_id='UCfb1' AND video_id='vid-from-sidecar'",
            )
            .fetch_one(&pool)
            .await
            .unwrap_or(0);
            n == 1
        })
    })
    .await;
    handle.abort();

    // The source should look like a successful poll: errors cleared,
    // last_success_at set, and the sidecar fallback timestamp written
    // so a future tick sees the per-source cap.
    let (last_success, errs, fb): (Option<i64>, i64, Option<i64>) = sqlx::query_as(
        "SELECT rss_last_success_at, rss_consecutive_errors, last_sidecar_fallback_at \
           FROM channel_sync_state WHERE channel_id='UCfb1'",
    )
    .fetch_one(&app.pool)
    .await
    .unwrap();
    assert!(last_success.is_some(), "rss_last_success_at must be set");
    assert_eq!(errs, 0, "errors must be reset on fallback success");
    assert!(fb.is_some(), "last_sidecar_fallback_at must be persisted");
}

#[tokio::test]
async fn fallback_marks_dead_only_after_threshold_not_found() {
    // Debounced shelve: a single sidecar 404 should NOT push
    // next_poll_at 1 year out. Pre-seed `consecutive_errors` to
    // threshold-1 so the very next NotFound crosses the threshold
    // and shelves the row. (Going through three real cycles would
    // also work but takes longer; this exercises the same code
    // path with a shorter test.)
    let (app, rss_mock, sidecar_mock) = setup_fallback_test().await;

    feed_cache::upsert_channel(&app.pool, "UCdead")
        .await
        .unwrap();
    // Pre-seed: 2 prior consecutive errors (threshold is 3 today).
    sqlx::query(
        "UPDATE channel_sync_state SET rss_consecutive_errors = 2 \
          WHERE channel_id = 'UCdead'",
    )
    .execute(&app.pool)
    .await
    .unwrap();

    Mock::given(method("GET"))
        .and(path("/feeds/videos.xml"))
        .and(query_param("channel_id", "UCdead"))
        .respond_with(ResponseTemplate::new(404))
        .mount(&rss_mock)
        .await;

    // Sidecar returns 404 with the "channel not found" payload (the
    // shape `handleGetChannel` emits when youtubei.js returns null).
    Mock::given(method("GET"))
        .and(path("/channel-videos/UCdead"))
        .respond_with(ResponseTemplate::new(404).set_body_json(json!({
            "error": "channel not found"
        })))
        .mount(&sidecar_mock)
        .await;

    let handle = tokio::spawn(feed_refresher::run(app.pool.clone()));

    let pool = app.pool.clone();
    wait_until("source classified as dead after threshold", move || {
        let pool = pool.clone();
        Box::pin(async move {
            let next: i64 = sqlx::query_scalar(
                "SELECT rss_next_poll_at FROM channel_sync_state \
                 WHERE channel_id='UCdead'",
            )
            .fetch_one(&pool)
            .await
            .unwrap_or(0);
            next > unix_now() + (30 * 24 * 60 * 60)
        })
    })
    .await;
    handle.abort();

    let (err, errs): (Option<String>, i64) = sqlx::query_as(
        "SELECT rss_last_error, rss_consecutive_errors FROM channel_sync_state \
         WHERE channel_id='UCdead'",
    )
    .fetch_one(&app.pool)
    .await
    .unwrap();
    assert!(
        err.as_deref().is_some_and(|s| s.contains("not found")),
        "expected 'not found' in last_error, got {err:?}"
    );
    assert_eq!(errs, 0, "dead path must clear the error counter");
}

/// Companion to the previous test: a first-time NotFound (no prior
/// errors) should *not* shelve the source — instead it should bump
/// `consecutive_errors` and reschedule with the normal backoff. This
/// protects channels that briefly 404 during YouTube-side glitches.
#[tokio::test]
async fn fallback_first_not_found_backs_off_does_not_shelve() {
    let (app, rss_mock, sidecar_mock) = setup_fallback_test().await;

    feed_cache::upsert_channel(&app.pool, "UCmaybe-dead")
        .await
        .unwrap();
    // No pre-seed: consecutive_errors starts at 0, so this 404 is
    // the *first* in a row.

    Mock::given(method("GET"))
        .and(path("/feeds/videos.xml"))
        .and(query_param("channel_id", "UCmaybe-dead"))
        .respond_with(ResponseTemplate::new(404))
        .mount(&rss_mock)
        .await;
    Mock::given(method("GET"))
        .and(path("/channel-videos/UCmaybe-dead"))
        .respond_with(
            ResponseTemplate::new(404).set_body_json(json!({"error": "channel not found"})),
        )
        .mount(&sidecar_mock)
        .await;

    let handle = tokio::spawn(feed_refresher::run(app.pool.clone()));

    // Wait for the row to be processed: consecutive_errors should
    // tick up from 0 to 1 *without* next_poll_at jumping a year.
    let pool = app.pool.clone();
    wait_until("first NotFound increments error counter", move || {
        let pool = pool.clone();
        Box::pin(async move {
            let errs: i64 = sqlx::query_scalar(
                "SELECT rss_consecutive_errors FROM channel_sync_state \
                 WHERE channel_id='UCmaybe-dead'",
            )
            .fetch_one(&pool)
            .await
            .unwrap_or(0);
            errs >= 1
        })
    })
    .await;
    handle.abort();

    let (next, errs, err): (i64, i64, Option<String>) = sqlx::query_as(
        "SELECT rss_next_poll_at, rss_consecutive_errors, rss_last_error FROM channel_sync_state \
         WHERE channel_id='UCmaybe-dead'",
    )
    .fetch_one(&app.pool)
    .await
    .unwrap();
    assert!(
        next < unix_now() + (30 * 24 * 60 * 60),
        "first NotFound must back off, not shelve; got next_poll_at = {next}"
    );
    assert_eq!(
        errs, 1,
        "consecutive_errors should be 1 after first NotFound"
    );
    let err = err.unwrap();
    assert!(
        err.contains("not found") && err.contains("1/"),
        "diagnostic should reflect the running count; got: {err}"
    );
}

#[tokio::test]
async fn fallback_soft_fails_when_sidecar_5xxs() {
    let (app, rss_mock, sidecar_mock) = setup_fallback_test().await;

    feed_cache::upsert_channel(&app.pool, "UCsoft")
        .await
        .unwrap();

    Mock::given(method("GET"))
        .and(path("/feeds/videos.xml"))
        .and(query_param("channel_id", "UCsoft"))
        .respond_with(ResponseTemplate::new(404))
        .mount(&rss_mock)
        .await;

    // Sidecar 500. The refresher must *not* classify the source as
    // dead — it should fall back to the standard
    // `record_poll_failure` path so a transient sidecar error doesn't
    // shelve real channels for a year.
    Mock::given(method("GET"))
        .and(path_regex(r"^/channel-videos/UCsoft.*"))
        .respond_with(ResponseTemplate::new(500).set_body_string("upstream boom"))
        .mount(&sidecar_mock)
        .await;

    let handle = tokio::spawn(feed_refresher::run(app.pool.clone()));

    let pool = app.pool.clone();
    wait_until(
        "rss_consecutive_errors increments and rss_last_error is set",
        move || {
            let pool = pool.clone();
            Box::pin(async move {
                let (errs, err): (i64, Option<String>) = sqlx::query_as(
                    "SELECT rss_consecutive_errors, rss_last_error FROM channel_sync_state \
                 WHERE channel_id='UCsoft'",
                )
                .fetch_one(&pool)
                .await
                .unwrap_or((0, None));
                errs >= 1 && err.is_some()
            })
        },
    )
    .await;
    handle.abort();

    let (next, err): (i64, Option<String>) = sqlx::query_as(
        "SELECT rss_next_poll_at, rss_last_error FROM channel_sync_state \
         WHERE channel_id='UCsoft'",
    )
    .fetch_one(&app.pool)
    .await
    .unwrap();
    // Backoff, not the 1-year deferral.
    assert!(
        next < unix_now() + (30 * 24 * 60 * 60),
        "soft-fail must use exponential backoff, not the dead-source defer"
    );
    let err = err.unwrap();
    assert!(
        err.contains("rss") && err.contains("sidecar"),
        "last_error should combine both transports; got: {err}"
    );
    // The sidecar's upstream status should surface in the message —
    // we order status-checks before body parsing so a non-JSON 5xx
    // body doesn't shadow the status code.
    assert!(
        err.contains("500"),
        "last_error should surface the upstream status; got: {err}"
    );
}

fn unix_now() -> i64 {
    chrono::Utc::now().timestamp()
}
