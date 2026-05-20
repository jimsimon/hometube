//! Integration test: the RSS poll path writes into `feed_source_items`
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
use hometube::services::feed_refresher::{self, KEY_CHANNEL_INTERVAL_S, KEY_DISPATCH_DELAY_MS};
use hometube::services::setup::set_config_value;
use hometube::services::youtube_rss::{self, PollOutcome, KEY_RSS_BASE_URL};
use wiremock::matchers::{method, path, query_param};
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
    feed_cache::upsert_source(&app.pool, "channel", "UCtest")
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
            feed_cache::replace_source_items(&app.pool, "channel", "UCtest", &items, 1_718_531_999)
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
    feed_cache::upsert_source(&app.pool, "channel", "UC304")
        .await
        .unwrap();
    feed_cache::replace_source_items(
        &app.pool,
        "channel",
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

    let count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM feed_source_items \
         WHERE kind='channel' AND source_id='UC304'",
    )
    .fetch_one(&app.pool)
    .await
    .unwrap();
    assert_eq!(count, 1, "304 must not clear cached items");
}

/// End-to-end test of the actual `feed_refresher::run` loop. Unlike
/// the earlier tests which call `poll_channel` directly, this one
/// spawns the production background task with a tight dispatch
/// interval, lets it pick up a seeded `feed_sources` row, and asserts
/// the row's items appear in `feed_source_items` afterwards.
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

    feed_cache::upsert_source(&app.pool, "channel", "UCloop")
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
            "SELECT COUNT(*) FROM feed_source_items \
             WHERE kind='channel' AND source_id='UCloop' AND video_id='loop-vid'",
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

    // The source bookkeeping should reflect a successful poll: last_success_at
    // set, no error, consecutive_errors reset.
    let (ls, err, errs): (Option<i64>, Option<String>, i64) = sqlx::query_as(
        "SELECT last_success_at, last_error, consecutive_errors \
           FROM feed_sources WHERE kind='channel' AND source_id='UCloop'",
    )
    .fetch_one(&app.pool)
    .await
    .unwrap();
    assert!(ls.is_some(), "last_success_at must be set after a poll");
    assert!(err.is_none(), "no error expected, got {err:?}");
    assert_eq!(errs, 0);
}
