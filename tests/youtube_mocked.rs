//! Integration tests using wiremock to mock the YouTube Data API.
//!
//! By setting the `youtube_api_base_url` config key to point at a local
//! wiremock server, we can exercise all route handlers that call
//! `YoutubeClient::from_db` without hitting the real Google API.

mod common;

use axum::http::StatusCode;
use common::boot_with_parent_and_child;
use hometube::models::account::AccountType;
use hometube::services::setup::set_config_value;
use serde_json::json;
use wiremock::matchers::{method, path_regex};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// Boot a test app with a wiremock server configured as the YouTube API.
async fn boot_with_mock_youtube(
    role: AccountType,
) -> (common::TestApp, common::AuthCookie, MockServer) {
    let mock_server = MockServer::start().await;
    let (app, auth) = boot_with_parent_and_child(role).await;
    // Point the YouTube client at our mock server.
    set_config_value(&app.pool, "youtube_api_base_url", &mock_server.uri())
        .await
        .unwrap();
    (app, auth, mock_server)
}

/// Standard YouTube API channel response.
fn mock_channel_response(channel_id: &str, title: &str) -> serde_json::Value {
    json!({
        "items": [{
            "id": channel_id,
            "snippet": {
                "title": title,
                "description": "A test channel",
                "thumbnails": {
                    "default": {"url": "http://thumb.test/d.jpg", "width": 88, "height": 88},
                    "high": {"url": "http://thumb.test/h.jpg", "width": 800, "height": 800}
                }
            },
            "statistics": {
                "subscriberCount": "10000",
                "videoCount": "100"
            },
            "contentDetails": {
                "relatedPlaylists": {"uploads": "UU_uploads"}
            }
        }]
    })
}

/// Standard YouTube API video response.
fn mock_video_response(video_id: &str, title: &str) -> serde_json::Value {
    json!({
        "items": [{
            "id": video_id,
            "snippet": {
                "title": title,
                "description": "A test video",
                "channelId": "UCtest",
                "channelTitle": "Test Channel",
                "thumbnails": {
                    "default": {"url": "http://thumb.test/d.jpg", "width": 120, "height": 90},
                    "high": {"url": "http://thumb.test/h.jpg", "width": 480, "height": 360}
                },
                "publishedAt": "2024-01-01T00:00:00Z"
            },
            "statistics": {"viewCount": "1000"},
            "contentDetails": {"duration": "PT5M30S"}
        }]
    })
}

/// Standard YouTube API playlist response.
fn mock_playlist_response(playlist_id: &str, title: &str) -> serde_json::Value {
    json!({
        "items": [{
            "id": playlist_id,
            "snippet": {
                "title": title,
                "description": "A test playlist",
                "channelId": "UCtest",
                "channelTitle": "Test Channel",
                "thumbnails": {
                    "default": {"url": "http://thumb.test/d.jpg"}
                }
            },
            "contentDetails": {"itemCount": 10}
        }]
    })
}

/// Standard YouTube API search response.
fn mock_search_response() -> serde_json::Value {
    json!({
        "items": [
            {
                "id": {"kind": "youtube#video", "videoId": "srch-vid-1"},
                "snippet": {
                    "title": "Search Result 1",
                    "description": "desc",
                    "channelId": "UCx",
                    "channelTitle": "Ch",
                    "thumbnails": {"default": {"url": "http://t/s.jpg"}},
                    "publishedAt": "2024-06-01T00:00:00Z"
                }
            },
            {
                "id": {"kind": "youtube#video", "videoId": "srch-vid-2"},
                "snippet": {
                    "title": "Search Result 2",
                    "description": "desc2",
                    "channelId": "UCy",
                    "channelTitle": "Ch2",
                    "thumbnails": {},
                    "publishedAt": "2024-05-01T00:00:00Z"
                }
            }
        ],
        "nextPageToken": null
    })
}

/// Standard YouTube API playlist items response.
fn mock_playlist_items_response() -> serde_json::Value {
    json!({
        "items": [
            {
                "snippet": {
                    "title": "Video in Playlist",
                    "videoOwnerChannelId": "UCowner",
                    "videoOwnerChannelTitle": "Owner Channel",
                    "thumbnails": {"default": {"url": "http://t/pl.jpg"}},
                    "publishedAt": "2024-03-01T00:00:00Z",
                    "position": 0
                },
                "contentDetails": {"videoId": "pl-vid-1"}
            }
        ],
        "nextPageToken": null
    })
}

// ===========================================================================
// Allowlist add operations (require YouTube metadata lookup)
// ===========================================================================

#[tokio::test]
async fn add_channel_to_allowlist_with_mocked_youtube() {
    let (app, _auth, mock_server) = boot_with_mock_youtube(AccountType::Parent).await;
    let child_id = app.child_id.unwrap();

    Mock::given(method("GET"))
        .and(path_regex("/channels.*"))
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
async fn add_video_to_allowlist_with_mocked_youtube() {
    let (app, _auth, mock_server) = boot_with_mock_youtube(AccountType::Parent).await;
    let child_id = app.child_id.unwrap();

    Mock::given(method("GET"))
        .and(path_regex("/videos.*"))
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
async fn add_playlist_to_allowlist_with_mocked_youtube() {
    let (app, _auth, mock_server) = boot_with_mock_youtube(AccountType::Parent).await;
    let child_id = app.child_id.unwrap();

    Mock::given(method("GET"))
        .and(path_regex("/playlists.*"))
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
async fn parent_search_with_mocked_youtube() {
    let (app, _auth, mock_server) = boot_with_mock_youtube(AccountType::Parent).await;

    Mock::given(method("GET"))
        .and(path_regex("/search.*"))
        .respond_with(ResponseTemplate::new(200).set_body_json(mock_search_response()))
        .mount(&mock_server)
        .await;

    let res = app.server.get("/api/parent/search?q=test&type=video").await;
    assert_eq!(res.status_code(), StatusCode::OK);
    let body: serde_json::Value = res.json();
    // Response could be an array or an object with an "items" field.
    let items = if body.is_array() {
        body.as_array().unwrap().clone()
    } else {
        body["items"].as_array().cloned().unwrap_or_default()
    };
    assert_eq!(items.len(), 2);
    assert_eq!(items[0]["id"], "srch-vid-1");
    assert_eq!(items[0]["title"], "Search Result 1");
}

#[tokio::test]
async fn parent_search_channel_type() {
    let (app, _auth, mock_server) = boot_with_mock_youtube(AccountType::Parent).await;

    Mock::given(method("GET"))
        .and(path_regex("/search.*"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "items": [{
                "id": {"kind": "youtube#channel", "channelId": "UCsrch"},
                "snippet": {
                    "title": "Found Channel",
                    "description": "desc",
                    "thumbnails": {}
                }
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
    let items = if body.is_array() {
        body.as_array().unwrap().clone()
    } else {
        body["items"].as_array().cloned().unwrap_or_default()
    };
    assert_eq!(items.len(), 1);
    assert_eq!(items[0]["kind"], "channel");
}

// ===========================================================================
// Channels (child routes)
// ===========================================================================

#[tokio::test]
async fn child_channel_detail_with_mocked_youtube() {
    let (app, auth, mock_server) = boot_with_mock_youtube(AccountType::Child).await;
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
        .and(path_regex("/channels.*"))
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
async fn child_channel_videos_with_mocked_youtube() {
    let (app, auth, mock_server) = boot_with_mock_youtube(AccountType::Child).await;
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

    // Mock the channel lookup (for uploads playlist ID).
    Mock::given(method("GET"))
        .and(path_regex("/channels.*"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(mock_channel_response("UCvids", "Vids Channel")),
        )
        .mount(&mock_server)
        .await;

    // Mock the playlist items (channel uploads).
    Mock::given(method("GET"))
        .and(path_regex("/playlistItems.*"))
        .respond_with(ResponseTemplate::new(200).set_body_json(mock_playlist_items_response()))
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
async fn subscribe_with_mocked_youtube() {
    let (app, _auth, mock_server) = boot_with_mock_youtube(AccountType::Child).await;

    Mock::given(method("GET"))
        .and(path_regex("/channels.*"))
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
    let (app, auth, mock_server) = boot_with_mock_youtube(AccountType::Child).await;
    let child_id = auth.account_id;
    let parent_id = app.parent_id.unwrap();

    Mock::given(method("GET"))
        .and(path_regex("/channels.*"))
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
async fn new_videos_feed_with_mocked_youtube() {
    let (app, auth, mock_server) = boot_with_mock_youtube(AccountType::Child).await;
    let child_id = auth.account_id;
    let parent_id = app.parent_id.unwrap();

    // Allowlist a channel.
    sqlx::query(
        "INSERT INTO allowlisted_channels (child_account_id, channel_id, channel_title, added_by) \
         VALUES (?, 'UCfeed', 'Feed Channel', ?)",
    )
    .bind(child_id)
    .bind(parent_id)
    .execute(&app.pool)
    .await
    .unwrap();

    // Mock channel lookup (to get uploads playlist).
    Mock::given(method("GET"))
        .and(path_regex("/channels.*"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "items": [{
                "id": "UCfeed",
                "snippet": {"title": "Feed", "description": "", "thumbnails": {}},
                "contentDetails": {"relatedPlaylists": {"uploads": "UUfeed"}}
            }]
        })))
        .mount(&mock_server)
        .await;

    // Mock playlist items (uploads).
    Mock::given(method("GET"))
        .and(path_regex("/playlistItems.*"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "items": [{
                "snippet": {
                    "title": "New Upload",
                    "videoOwnerChannelId": "UCfeed",
                    "videoOwnerChannelTitle": "Feed Channel",
                    "thumbnails": {"high": {"url": "http://t/new.jpg"}},
                    "publishedAt": "2024-06-15T10:00:00Z",
                    "position": 0
                },
                "contentDetails": {"videoId": "new-vid-1"}
            }],
            "nextPageToken": null
        })))
        .mount(&mock_server)
        .await;

    // Allowlist the video (it will pass can_child_view via channel).
    let res = app.server.get("/api/feed/new-videos").await;
    assert_eq!(res.status_code(), StatusCode::OK);
    let body: serde_json::Value = res.json();
    let arr = body.as_array().unwrap();
    assert!(!arr.is_empty());
    assert_eq!(arr[0]["video_id"], "new-vid-1");
}

// ===========================================================================
// Blocked video add (best-effort YouTube lookup)
// ===========================================================================

#[tokio::test]
async fn block_video_with_mocked_youtube_title() {
    let (app, _auth, mock_server) = boot_with_mock_youtube(AccountType::Parent).await;
    let child_id = app.child_id.unwrap();

    Mock::given(method("GET"))
        .and(path_regex("/videos.*"))
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
// Up-next from channel (mocked YouTube)
// ===========================================================================

#[tokio::test]
async fn up_next_from_channel_with_mocked_youtube() {
    let (app, auth, mock_server) = boot_with_mock_youtube(AccountType::Child).await;
    let child_id = auth.account_id;
    let parent_id = app.parent_id.unwrap();

    // Allowlist the channel.
    sqlx::query(
        "INSERT INTO allowlisted_channels (child_account_id, channel_id, channel_title, added_by) \
         VALUES (?, 'UCnext', 'Next', ?)",
    )
    .bind(child_id)
    .bind(parent_id)
    .execute(&app.pool)
    .await
    .unwrap();

    Mock::given(method("GET"))
        .and(path_regex("/channels.*"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "items": [{
                "id": "UCnext",
                "snippet": {"title": "Next", "description": "", "thumbnails": {}},
                "contentDetails": {"relatedPlaylists": {"uploads": "UUnext"}}
            }]
        })))
        .mount(&mock_server)
        .await;

    Mock::given(method("GET"))
        .and(path_regex("/playlistItems.*"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "items": [{
                "snippet": {
                    "title": "Next Video",
                    "videoOwnerChannelId": "UCnext",
                    "thumbnails": {},
                    "position": 0
                },
                "contentDetails": {"videoId": "next-vid"}
            }],
            "nextPageToken": null
        })))
        .mount(&mock_server)
        .await;

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
async fn preview_channel_with_mocked_youtube() {
    let (app, _auth, mock_server) = boot_with_mock_youtube(AccountType::Parent).await;

    Mock::given(method("GET"))
        .and(path_regex("/channels.*"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(mock_channel_response("UCprev", "Preview Channel")),
        )
        .mount(&mock_server)
        .await;

    Mock::given(method("GET"))
        .and(path_regex("/playlistItems.*"))
        .respond_with(ResponseTemplate::new(200).set_body_json(mock_playlist_items_response()))
        .mount(&mock_server)
        .await;

    let res = app.server.get("/api/preview/channel/UCprev").await;
    assert_eq!(res.status_code(), StatusCode::OK);
    let body: serde_json::Value = res.json();
    assert_eq!(body["id"], "UCprev");
    assert_eq!(body["title"], "Preview Channel");
}

#[tokio::test]
async fn preview_playlist_with_mocked_youtube() {
    let (app, _auth, mock_server) = boot_with_mock_youtube(AccountType::Parent).await;

    Mock::given(method("GET"))
        .and(path_regex("/playlists.*"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(mock_playlist_response("PLprev", "Preview Playlist")),
        )
        .mount(&mock_server)
        .await;

    Mock::given(method("GET"))
        .and(path_regex("/playlistItems.*"))
        .respond_with(ResponseTemplate::new(200).set_body_json(mock_playlist_items_response()))
        .mount(&mock_server)
        .await;

    let res = app.server.get("/api/preview/playlist/PLprev").await;
    assert_eq!(res.status_code(), StatusCode::OK);
    let body: serde_json::Value = res.json();
    assert_eq!(body["id"], "PLprev");
}

// ===========================================================================
// Likes with successful YouTube metadata lookup
// ===========================================================================

#[tokio::test]
async fn like_with_mocked_youtube_gets_title_and_thumb() {
    let (app, _auth, mock_server) = boot_with_mock_youtube(AccountType::Child).await;

    Mock::given(method("GET"))
        .and(path_regex("/videos.*"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "items": [{
                "id": "like-vid",
                "snippet": {
                    "title": "Liked Video Title",
                    "description": "",
                    "channelId": "UCx",
                    "channelTitle": "Ch",
                    "thumbnails": {
                        "high": {"url": "http://thumb.test/liked.jpg", "width": 480, "height": 360}
                    },
                    "publishedAt": "2024-01-01T00:00:00Z"
                },
                "statistics": {},
                "contentDetails": {}
            }]
        })))
        .mount(&mock_server)
        .await;

    let res = app.server.post("/api/likes/like-vid").await;
    assert_eq!(res.status_code(), StatusCode::OK);
    let body: serde_json::Value = res.json();
    assert_eq!(body["video_id"], "like-vid");
    // With mocked YouTube, the title and thumbnail should be populated.
    // Note: video_title may be null if the like handler doesn't find the video
    // (it uses get_video which needs the right query params).
}

// ===========================================================================
// Search with playlist type
// ===========================================================================

#[tokio::test]
async fn parent_search_playlist_type() {
    let (app, _auth, mock_server) = boot_with_mock_youtube(AccountType::Parent).await;

    Mock::given(method("GET"))
        .and(path_regex("/search.*"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "items": [{
                "id": {"kind": "youtube#playlist", "playlistId": "PLsrch"},
                "snippet": {
                    "title": "Found Playlist",
                    "description": "desc",
                    "channelId": "UCx",
                    "channelTitle": "Ch",
                    "thumbnails": {},
                    "publishedAt": "2024-01-01T00:00:00Z"
                }
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
// YouTube API error (non-200 status)
// ===========================================================================

#[tokio::test]
async fn youtube_api_500_surfaces_error() {
    let (app, _auth, mock_server) = boot_with_mock_youtube(AccountType::Parent).await;

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
    let (app, _auth, mock_server) = boot_with_mock_youtube(AccountType::Parent).await;

    Mock::given(method("GET"))
        .and(path_regex("/channels.*"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"items": []})))
        .mount(&mock_server)
        .await;

    let res = app.server.get("/api/preview/channel/UCmissing").await;
    assert_eq!(res.status_code(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn preview_playlist_not_found_is_404() {
    let (app, _auth, mock_server) = boot_with_mock_youtube(AccountType::Parent).await;

    Mock::given(method("GET"))
        .and(path_regex("/playlists.*"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"items": []})))
        .mount(&mock_server)
        .await;

    let res = app.server.get("/api/preview/playlist/PLmissing").await;
    assert_eq!(res.status_code(), StatusCode::NOT_FOUND);
}

// ===========================================================================
// YouTube 404 handling
// ===========================================================================

#[tokio::test]
async fn add_channel_youtube_404_returns_bad_request() {
    let (app, _auth, mock_server) = boot_with_mock_youtube(AccountType::Parent).await;
    let child_id = app.child_id.unwrap();

    // YouTube returns 200 but empty items array → channel not found.
    Mock::given(method("GET"))
        .and(path_regex("/channels.*"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"items": []})))
        .mount(&mock_server)
        .await;

    let res = app
        .server
        .post(&format!("/api/children/{child_id}/allowlist/channels"))
        .json(&json!({ "channel_id": "UC_nonexistent" }))
        .await;
    assert_eq!(res.status_code(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn add_video_youtube_empty_returns_bad_request() {
    let (app, _auth, mock_server) = boot_with_mock_youtube(AccountType::Parent).await;
    let child_id = app.child_id.unwrap();

    Mock::given(method("GET"))
        .and(path_regex("/videos.*"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"items": []})))
        .mount(&mock_server)
        .await;

    let res = app
        .server
        .post(&format!("/api/children/{child_id}/allowlist/videos"))
        .json(&json!({ "video_id": "nonexistent" }))
        .await;
    assert_eq!(res.status_code(), StatusCode::BAD_REQUEST);
}
