//! Integration tests using wiremock to mock the discovery sidecar.
//!
//! By setting the `discovery_sidecar_url` config key to point at a local
//! wiremock server, we can exercise all route handlers that call
//! `YoutubeClient::from_db` without hitting the real sidecar or YouTube.

mod common;

use axum::http::StatusCode;
use common::boot_with_parent_and_child;
use hometube::models::account::AccountType;
use hometube::services::setup::set_config_value;
use serde_json::json;
use wiremock::matchers::{method, path, path_regex};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// Boot a test app with a wiremock server configured as the discovery sidecar.
async fn boot_with_mock_discovery(
    role: AccountType,
) -> (common::TestApp, common::AuthCookie, MockServer) {
    let mock_server = MockServer::start().await;
    let (app, auth) = boot_with_parent_and_child(role).await;
    // Point the YoutubeClient at our mock sidecar.
    set_config_value(&app.pool, "discovery_sidecar_url", &mock_server.uri())
        .await
        .unwrap();
    (app, auth, mock_server)
}

/// Mock sidecar channel response.
fn mock_channel_response(channel_id: &str, title: &str) -> serde_json::Value {
    json!({
        "id": channel_id,
        "title": title,
        "description": "A test channel",
        "thumbnails": {
            "default": {"url": "http://thumb.test/d.jpg", "width": 88, "height": 88},
            "high": {"url": "http://thumb.test/h.jpg", "width": 800, "height": 800}
        },
        "subscriber_count": 10000,
        "video_count": 100,
        "uploads_playlist_id": "UU_uploads"
    })
}

/// Mock sidecar video response.
fn mock_video_response(video_id: &str, title: &str) -> serde_json::Value {
    json!({
        "id": video_id,
        "title": title,
        "description": "A test video",
        "channel_id": "UCtest",
        "channel_title": "Test Channel",
        "thumbnails": {
            "default": {"url": "http://thumb.test/d.jpg", "width": 120, "height": 90},
            "high": {"url": "http://thumb.test/h.jpg", "width": 480, "height": 360}
        },
        "published_at": "2024-01-01T00:00:00Z",
        "duration": "PT5M30S",
        "view_count": 1000
    })
}

/// Mock sidecar playlist response.
fn mock_playlist_response(playlist_id: &str, title: &str) -> serde_json::Value {
    json!({
        "id": playlist_id,
        "title": title,
        "description": "A test playlist",
        "channel_id": "UCtest",
        "channel_title": "Test Channel",
        "thumbnails": {
            "default": {"url": "http://thumb.test/d.jpg"}
        },
        "item_count": 10
    })
}

/// Mock sidecar search response.
fn mock_search_response() -> serde_json::Value {
    json!({
        "items": [
            {
                "kind": "video",
                "id": "srch-vid-1",
                "title": "Search Result 1",
                "description": "desc",
                "channel_id": "UCx",
                "channel_title": "Ch",
                "thumbnails": {"default": {"url": "http://t/s.jpg"}},
                "published_at": "2024-06-01T00:00:00Z"
            },
            {
                "kind": "video",
                "id": "srch-vid-2",
                "title": "Search Result 2",
                "description": "desc2",
                "channel_id": "UCy",
                "channel_title": "Ch2",
                "thumbnails": {},
                "published_at": "2024-05-01T00:00:00Z"
            }
        ]
    })
}

/// Mock sidecar channel-videos / playlist-items response.
fn mock_video_items_response() -> serde_json::Value {
    json!({
        "items": [
            {
                "video_id": "pl-vid-1",
                "title": "Video in Playlist",
                "channel_id": "UCowner",
                "channel_title": "Owner Channel",
                "thumbnails": {"default": {"url": "http://t/pl.jpg"}},
                "published_at": "2024-03-01T00:00:00Z",
                "position": 0
            }
        ],
        "next_page_token": null
    })
}

// ===========================================================================
// Allowlist add operations (require discovery metadata lookup)
// ===========================================================================

#[tokio::test]
async fn add_channel_to_allowlist_with_mocked_discovery() {
    let (app, _auth, mock_server) = boot_with_mock_discovery(AccountType::Parent).await;
    let child_id = app.child_id.unwrap();

    Mock::given(method("GET"))
        .and(path("/channels/UCmocked"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(mock_channel_response("UCmocked", "Mocked Channel")),
        )
        .mount(&mock_server)
        .await;

    let res = app
        .server
        .post(&format!("/api/children/{child_id}/allowlist/channels"))
        .json(&json!({ "channel_id": "UCmocked" }))
        .await;
    assert_eq!(res.status_code(), StatusCode::OK);
    let body: serde_json::Value = res.json();
    assert_eq!(body["channel_id"], "UCmocked");
    assert_eq!(body["channel_title"], "Mocked Channel");
}

#[tokio::test]
async fn add_video_to_allowlist_with_mocked_discovery() {
    let (app, _auth, mock_server) = boot_with_mock_discovery(AccountType::Parent).await;
    let child_id = app.child_id.unwrap();

    Mock::given(method("GET"))
        .and(path("/videos/vid-mocked"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(mock_video_response("vid-mocked", "Mocked Video")),
        )
        .mount(&mock_server)
        .await;

    let res = app
        .server
        .post(&format!("/api/children/{child_id}/allowlist/videos"))
        .json(&json!({ "video_id": "vid-mocked" }))
        .await;
    assert_eq!(res.status_code(), StatusCode::OK);
    let body: serde_json::Value = res.json();
    assert_eq!(body["video_id"], "vid-mocked");
    assert_eq!(body["video_title"], "Mocked Video");
}

#[tokio::test]
async fn add_playlist_to_allowlist_with_mocked_discovery() {
    let (app, _auth, mock_server) = boot_with_mock_discovery(AccountType::Parent).await;
    let child_id = app.child_id.unwrap();

    Mock::given(method("GET"))
        .and(path("/playlists/PLmocked"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(mock_playlist_response("PLmocked", "Mocked Playlist")),
        )
        .mount(&mock_server)
        .await;

    let res = app
        .server
        .post(&format!("/api/children/{child_id}/allowlist/playlists"))
        .json(&json!({ "playlist_id": "PLmocked" }))
        .await;
    assert_eq!(res.status_code(), StatusCode::OK);
    let body: serde_json::Value = res.json();
    assert_eq!(body["playlist_id"], "PLmocked");
    assert_eq!(body["playlist_title"], "Mocked Playlist");
}

// ===========================================================================
// Search
// ===========================================================================

#[tokio::test]
async fn parent_search_with_mocked_discovery() {
    let (app, _auth, mock_server) = boot_with_mock_discovery(AccountType::Parent).await;

    Mock::given(method("GET"))
        .and(path_regex("/search.*"))
        .respond_with(ResponseTemplate::new(200).set_body_json(mock_search_response()))
        .mount(&mock_server)
        .await;

    let res = app.server.get("/api/parent/search?q=test&type=video").await;
    assert_eq!(res.status_code(), StatusCode::OK);
    let body: serde_json::Value = res.json();
    let items = body["items"].as_array().cloned().unwrap_or_default();
    assert_eq!(items.len(), 2);
    assert_eq!(items[0]["id"], "srch-vid-1");
    assert_eq!(items[0]["title"], "Search Result 1");
}

#[tokio::test]
async fn parent_search_channel_type() {
    let (app, _auth, mock_server) = boot_with_mock_discovery(AccountType::Parent).await;

    Mock::given(method("GET"))
        .and(path_regex("/search.*"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "items": [{
                "kind": "channel",
                "id": "UCsrch",
                "title": "Found Channel",
                "description": "desc",
                "thumbnails": {}
            }]
        })))
        .mount(&mock_server)
        .await;

    let res = app
        .server
        .get("/api/parent/search?q=channels&type=channel")
        .await;
    assert_eq!(res.status_code(), StatusCode::OK);
    let body: serde_json::Value = res.json();
    let items = body["items"].as_array().cloned().unwrap_or_default();
    assert_eq!(items.len(), 1);
    assert_eq!(items[0]["kind"], "channel");
}

// ===========================================================================
// Channels (child routes)
// ===========================================================================

#[tokio::test]
async fn child_channel_detail_with_mocked_discovery() {
    let (app, auth, mock_server) = boot_with_mock_discovery(AccountType::Child).await;
    let child_id = auth.account_id;
    let parent_id = app.parent_id.unwrap();

    // Allowlist the channel.
    sqlx::query(
        "INSERT INTO allowlisted_channels (child_account_id, channel_id, channel_title, added_by) \
         VALUES (?, 'UCmocked', 'Mocked', ?)",
    )
    .bind(child_id)
    .bind(parent_id)
    .execute(&app.pool)
    .await
    .unwrap();

    Mock::given(method("GET"))
        .and(path("/channels/UCmocked"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(mock_channel_response("UCmocked", "Mocked Channel")),
        )
        .mount(&mock_server)
        .await;

    let res = app.server.get("/api/channels/UCmocked").await;
    assert_eq!(res.status_code(), StatusCode::OK);
    let body: serde_json::Value = res.json();
    assert_eq!(body["id"], "UCmocked");
    assert_eq!(body["title"], "Mocked Channel");
}

#[tokio::test]
async fn child_channel_videos_with_mocked_discovery() {
    let (app, auth, mock_server) = boot_with_mock_discovery(AccountType::Child).await;
    let child_id = auth.account_id;
    let parent_id = app.parent_id.unwrap();

    // Allowlist the channel.
    sqlx::query(
        "INSERT INTO allowlisted_channels (child_account_id, channel_id, channel_title, added_by) \
         VALUES (?, 'UCvids', 'Vids Channel', ?)",
    )
    .bind(child_id)
    .bind(parent_id)
    .execute(&app.pool)
    .await
    .unwrap();

    // Mock channel-videos endpoint (sidecar handles the uploads
    // playlist resolution internally).
    Mock::given(method("GET"))
        .and(path_regex("/channel-videos/UCvids.*"))
        .respond_with(ResponseTemplate::new(200).set_body_json(mock_video_items_response()))
        .mount(&mock_server)
        .await;

    // Allowlist the video that will appear.
    sqlx::query(
        "INSERT INTO allowlisted_videos (child_account_id, video_id, video_title, added_by) \
         VALUES (?, 'pl-vid-1', 'V', ?)",
    )
    .bind(child_id)
    .bind(parent_id)
    .execute(&app.pool)
    .await
    .unwrap();

    let res = app.server.get("/api/channels/UCvids/videos").await;
    assert_eq!(res.status_code(), StatusCode::OK);
    let body: serde_json::Value = res.json();
    let items = body["items"].as_array().unwrap();
    assert!(!items.is_empty());
}

// ===========================================================================
// Subscriptions
// ===========================================================================

#[tokio::test]
async fn subscribe_with_mocked_discovery() {
    let (app, _auth, mock_server) = boot_with_mock_discovery(AccountType::Child).await;

    Mock::given(method("GET"))
        .and(path("/channels/UCsub"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(mock_channel_response("UCsub", "Subscribed Channel")),
        )
        .mount(&mock_server)
        .await;

    let res = app
        .server
        .post("/api/subscriptions")
        .json(&json!({ "channel_id": "UCsub" }))
        .await;
    assert_eq!(res.status_code(), StatusCode::OK);
    let body: serde_json::Value = res.json();
    assert_eq!(body["channel_id"], "UCsub");
    assert_eq!(body["channel_title"], "Subscribed Channel");
}

#[tokio::test]
async fn subscribe_and_list_visibility() {
    let (app, auth, mock_server) = boot_with_mock_discovery(AccountType::Child).await;
    let child_id = auth.account_id;
    let parent_id = app.parent_id.unwrap();

    Mock::given(method("GET"))
        .and(path("/channels/UCvis"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(mock_channel_response("UCvis", "Visible")),
        )
        .mount(&mock_server)
        .await;

    // Subscribe (not yet allowlisted → visible=false).
    app.server
        .post("/api/subscriptions")
        .json(&json!({ "channel_id": "UCvis" }))
        .await;

    let res = app.server.get("/api/subscriptions").await;
    let body: serde_json::Value = res.json();
    let arr = body.as_array().unwrap();
    assert_eq!(arr[0]["visible"], false);

    // Now allowlist the channel.
    sqlx::query(
        "INSERT INTO allowlisted_channels (child_account_id, channel_id, channel_title, added_by) \
         VALUES (?, 'UCvis', 'V', ?)",
    )
    .bind(child_id)
    .bind(parent_id)
    .execute(&app.pool)
    .await
    .unwrap();

    let res = app.server.get("/api/subscriptions").await;
    let body: serde_json::Value = res.json();
    let arr = body.as_array().unwrap();
    assert_eq!(arr[0]["visible"], true);
}

// ===========================================================================
// Feed - new videos
// ===========================================================================

#[tokio::test]
async fn new_videos_feed_with_mocked_discovery() {
    // The new-videos feed now reads from the `feed_source_items` cache
    // populated by the background refresher. We bypass the refresher
    // here and seed the cache directly so we can exercise the handler
    // path in isolation.
    let (app, auth, _mock_server) = boot_with_mock_discovery(AccountType::Child).await;
    let child_id = auth.account_id;
    let parent_id = app.parent_id.unwrap();

    sqlx::query(
        "INSERT INTO allowlisted_channels (child_account_id, channel_id, channel_title, added_by) \
         VALUES (?, 'UCfeed', 'Feed Channel', ?)",
    )
    .bind(child_id)
    .bind(parent_id)
    .execute(&app.pool)
    .await
    .unwrap();

    hometube::services::feed_cache::upsert_source(&app.pool, "channel", "UCfeed")
        .await
        .unwrap();
    hometube::services::feed_cache::replace_source_items(
        &app.pool,
        "channel",
        "UCfeed",
        &[hometube::services::feed_cache::ItemRow {
            video_id: "new-vid-1".into(),
            title: "New Upload".into(),
            channel_id: Some("UCfeed".into()),
            channel_title: Some("Feed Channel".into()),
            thumbnail_url: Some("http://t/new.jpg".into()),
            published_at: Some(1_718_445_600),
            published_raw: Some("2024-06-15T10:00:00Z".into()),
        }],
        1_718_445_600,
    )
    .await
    .unwrap();

    let res = app.server.get("/api/feed/new-videos").await;
    assert_eq!(res.status_code(), StatusCode::OK);
    let body: serde_json::Value = res.json();
    let arr = body.as_array().unwrap();
    assert!(!arr.is_empty());
    assert_eq!(arr[0]["video_id"], "new-vid-1");
}

// ===========================================================================
// Blocked video add (best-effort discovery lookup)
// ===========================================================================

#[tokio::test]
async fn block_video_with_mocked_discovery_title() {
    let (app, _auth, mock_server) = boot_with_mock_discovery(AccountType::Parent).await;
    let child_id = app.child_id.unwrap();

    Mock::given(method("GET"))
        .and(path("/videos/vid-block-m"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(mock_video_response("vid-block-m", "Video To Block")),
        )
        .mount(&mock_server)
        .await;

    let res = app
        .server
        .post(&format!("/api/children/{child_id}/blocked"))
        .json(&json!({ "video_id": "vid-block-m", "reason": "scary" }))
        .await;
    assert_eq!(res.status_code(), StatusCode::OK);
    let body: serde_json::Value = res.json();
    assert_eq!(body["video_title"], "Video To Block");
    assert_eq!(body["reason"], "scary");
}

// ===========================================================================
// Up-next from channel (mocked discovery)
// ===========================================================================

#[tokio::test]
async fn up_next_from_channel_with_mocked_discovery() {
    // Up-next-by-channel now reads from the `feed_source_items` cache
    // populated by the background refresher (avoiding a sidecar
    // round-trip on every request). Seed the cache directly.
    let (app, auth, _mock_server) = boot_with_mock_discovery(AccountType::Child).await;
    let child_id = auth.account_id;
    let parent_id = app.parent_id.unwrap();

    sqlx::query(
        "INSERT INTO allowlisted_channels (child_account_id, channel_id, channel_title, added_by) \
         VALUES (?, 'UCnext', 'Next', ?)",
    )
    .bind(child_id)
    .bind(parent_id)
    .execute(&app.pool)
    .await
    .unwrap();

    hometube::services::feed_cache::upsert_source(&app.pool, "channel", "UCnext")
        .await
        .unwrap();
    hometube::services::feed_cache::replace_source_items(
        &app.pool,
        "channel",
        "UCnext",
        &[hometube::services::feed_cache::ItemRow {
            video_id: "next-vid".into(),
            title: "Next Video".into(),
            channel_id: Some("UCnext".into()),
            channel_title: Some("Next".into()),
            thumbnail_url: None,
            published_at: Some(1_700_000_000),
            published_raw: Some("2023-11-14T22:13:20Z".into()),
        }],
        1_700_000_000,
    )
    .await
    .unwrap();

    let res = app
        .server
        .get("/api/feed/up-next?from=channel:UCnext")
        .await;
    assert_eq!(res.status_code(), StatusCode::OK);
    let body: serde_json::Value = res.json();
    let arr = body.as_array().unwrap();
    assert!(!arr.is_empty());
}

// ===========================================================================
// Preview endpoints (parent only)
// ===========================================================================

#[tokio::test]
async fn preview_channel_with_mocked_discovery() {
    let (app, _auth, mock_server) = boot_with_mock_discovery(AccountType::Parent).await;

    Mock::given(method("GET"))
        .and(path("/channels/UCprev"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(mock_channel_response("UCprev", "Preview Channel")),
        )
        .mount(&mock_server)
        .await;

    Mock::given(method("GET"))
        .and(path_regex("/channel-videos/UCprev.*"))
        .respond_with(ResponseTemplate::new(200).set_body_json(mock_video_items_response()))
        .mount(&mock_server)
        .await;

    let res = app.server.get("/api/preview/channel/UCprev").await;
    assert_eq!(res.status_code(), StatusCode::OK);
    let body: serde_json::Value = res.json();
    assert_eq!(body["id"], "UCprev");
    assert_eq!(body["title"], "Preview Channel");
}

#[tokio::test]
async fn preview_playlist_with_mocked_discovery() {
    let (app, _auth, mock_server) = boot_with_mock_discovery(AccountType::Parent).await;

    Mock::given(method("GET"))
        .and(path("/playlists/PLprev"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(mock_playlist_response("PLprev", "Preview Playlist")),
        )
        .mount(&mock_server)
        .await;

    Mock::given(method("GET"))
        .and(path_regex("/playlist-items/PLprev.*"))
        .respond_with(ResponseTemplate::new(200).set_body_json(mock_video_items_response()))
        .mount(&mock_server)
        .await;

    let res = app.server.get("/api/preview/playlist/PLprev").await;
    assert_eq!(res.status_code(), StatusCode::OK);
    let body: serde_json::Value = res.json();
    assert_eq!(body["id"], "PLprev");
}

// ===========================================================================
// Search with playlist type
// ===========================================================================

#[tokio::test]
async fn parent_search_playlist_type() {
    let (app, _auth, mock_server) = boot_with_mock_discovery(AccountType::Parent).await;

    Mock::given(method("GET"))
        .and(path_regex("/search.*"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "items": [{
                "kind": "playlist",
                "id": "PLsrch",
                "title": "Found Playlist",
                "description": "desc",
                "channel_id": "UCx",
                "channel_title": "Ch",
                "thumbnails": {},
                "published_at": "2024-01-01T00:00:00Z"
            }]
        })))
        .mount(&mock_server)
        .await;

    let res = app
        .server
        .get("/api/parent/search?q=playlists&type=playlist")
        .await;
    assert_eq!(res.status_code(), StatusCode::OK);
}

// ===========================================================================
// Discovery sidecar error (non-200 status)
// ===========================================================================

#[tokio::test]
async fn discovery_sidecar_500_surfaces_error() {
    let (app, _auth, mock_server) = boot_with_mock_discovery(AccountType::Parent).await;

    Mock::given(method("GET"))
        .and(path_regex("/search.*"))
        .respond_with(ResponseTemplate::new(500).set_body_string("internal server error"))
        .mount(&mock_server)
        .await;

    let res = app.server.get("/api/parent/search?q=fail&type=video").await;
    let status = res.status_code().as_u16();
    assert!(status >= 400);
}

#[tokio::test]
async fn preview_channel_not_found_is_404() {
    let (app, _auth, mock_server) = boot_with_mock_discovery(AccountType::Parent).await;

    Mock::given(method("GET"))
        .and(path("/channels/UCmissing"))
        .respond_with(
            ResponseTemplate::new(404).set_body_json(json!({"error": "channel not found"})),
        )
        .mount(&mock_server)
        .await;

    let res = app.server.get("/api/preview/channel/UCmissing").await;
    let status = res.status_code().as_u16();
    assert!(status >= 400);
}

#[tokio::test]
async fn preview_playlist_not_found_is_404() {
    let (app, _auth, mock_server) = boot_with_mock_discovery(AccountType::Parent).await;

    Mock::given(method("GET"))
        .and(path("/playlists/PLmissing"))
        .respond_with(
            ResponseTemplate::new(404).set_body_json(json!({"error": "playlist not found"})),
        )
        .mount(&mock_server)
        .await;

    let res = app.server.get("/api/preview/playlist/PLmissing").await;
    let status = res.status_code().as_u16();
    assert!(status >= 400);
}

// ===========================================================================
// Discovery 404 handling
// ===========================================================================

#[tokio::test]
async fn add_channel_discovery_404_returns_bad_request() {
    let (app, _auth, mock_server) = boot_with_mock_discovery(AccountType::Parent).await;
    let child_id = app.child_id.unwrap();

    Mock::given(method("GET"))
        .and(path_regex("/channels/.*"))
        .respond_with(
            ResponseTemplate::new(404).set_body_json(json!({"error": "channel not found"})),
        )
        .mount(&mock_server)
        .await;

    let res = app
        .server
        .post(&format!("/api/children/{child_id}/allowlist/channels"))
        .json(&json!({ "channel_id": "UC_nonexistent" }))
        .await;
    let status = res.status_code().as_u16();
    assert!(status >= 400);
}

#[tokio::test]
async fn add_video_discovery_empty_returns_bad_request() {
    let (app, _auth, mock_server) = boot_with_mock_discovery(AccountType::Parent).await;
    let child_id = app.child_id.unwrap();

    Mock::given(method("GET"))
        .and(path_regex("/videos/.*"))
        .respond_with(ResponseTemplate::new(404).set_body_json(json!({"error": "video not found"})))
        .mount(&mock_server)
        .await;

    let res = app
        .server
        .post(&format!("/api/children/{child_id}/allowlist/videos"))
        .json(&json!({ "video_id": "nonexistent" }))
        .await;
    let status = res.status_code().as_u16();
    assert!(status >= 400);
}
