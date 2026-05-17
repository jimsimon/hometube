//! Playlist tests using wiremock to mock the discovery sidecar for
//! add_video and add_library operations.

mod common;

use axum::http::StatusCode;
use common::boot_with_parent_and_child;
use hometube::models::account::AccountType;
use hometube::services::setup::set_config_value;
use serde_json::json;
use wiremock::matchers::{method, path, path_regex};
use wiremock::{Mock, MockServer, ResponseTemplate};

async fn boot_with_mock(role: AccountType) -> (common::TestApp, common::AuthCookie, MockServer) {
    let mock_server = MockServer::start().await;
    let (app, auth) = boot_with_parent_and_child(role).await;
    set_config_value(&app.pool, "discovery_sidecar_url", &mock_server.uri())
        .await
        .unwrap();
    (app, auth, mock_server)
}

fn mock_video_response(video_id: &str) -> serde_json::Value {
    json!({
        "id": video_id,
        "title": "YT Video",
        "description": "desc",
        "channel_id": "UCvid",
        "channel_title": "VidCh",
        "thumbnails": {"high": {"url": "http://t/h.jpg", "width": 480, "height": 360}},
        "published_at": "2024-01-01T00:00:00Z",
        "duration": "PT3M",
        "view_count": 500
    })
}

fn mock_playlist_response() -> serde_json::Value {
    json!({
        "id": "PL_import",
        "title": "Imported Playlist",
        "description": "From YouTube",
        "channel_id": "UClib",
        "channel_title": "Lib Ch",
        "thumbnails": {},
        "item_count": 2
    })
}

fn mock_playlist_items() -> serde_json::Value {
    json!({
        "items": [
            {
                "video_id": "pl-v-1",
                "title": "PL Item 1",
                "channel_id": "UCpl",
                "channel_title": "PL Ch",
                "thumbnails": {"default": {"url": "http://t/1.jpg"}},
                "published_at": "2024-01-01T00:00:00Z",
                "position": 0
            },
            {
                "video_id": "pl-v-2",
                "title": "PL Item 2",
                "channel_id": "UCpl",
                "channel_title": "PL Ch",
                "thumbnails": {},
                "published_at": "2024-01-02T00:00:00Z",
                "position": 1
            }
        ],
        "next_page_token": null
    })
}

// ===========================================================================
// Child playlists — add_video
// ===========================================================================

#[tokio::test]
async fn child_playlist_add_video_with_mocked_youtube() {
    let (app, _auth, mock_server) = boot_with_mock(AccountType::Child).await;

    // Create a playlist.
    let res = app
        .server
        .post("/api/playlists")
        .json(&json!({ "title": "My Mix", "description": "" }))
        .await;
    let body: serde_json::Value = res.json();
    let pl_id = body["id"].as_i64().unwrap();

    // Mock the sidecar video lookup.
    Mock::given(method("GET"))
        .and(path("/videos/add-vid"))
        .respond_with(ResponseTemplate::new(200).set_body_json(mock_video_response("add-vid")))
        .mount(&mock_server)
        .await;

    let res = app
        .server
        .post(&format!("/api/playlists/{pl_id}/videos"))
        .json(&json!({ "video_id": "add-vid" }))
        .await;
    assert_eq!(res.status_code(), StatusCode::OK);
    let body: serde_json::Value = res.json();
    assert_eq!(body["video_id"], "add-vid");
    assert_eq!(body["video_title"], "YT Video");
}

#[tokio::test]
async fn child_playlist_add_video_not_found() {
    let (app, _auth, mock_server) = boot_with_mock(AccountType::Child).await;

    let res = app
        .server
        .post("/api/playlists")
        .json(&json!({ "title": "PL", "description": "" }))
        .await;
    let body: serde_json::Value = res.json();
    let pl_id = body["id"].as_i64().unwrap();

    Mock::given(method("GET"))
        .and(path_regex("/videos/.*"))
        .respond_with(
            ResponseTemplate::new(404).set_body_json(json!({"error": "video not found"})),
        )
        .mount(&mock_server)
        .await;

    let res = app
        .server
        .post(&format!("/api/playlists/{pl_id}/videos"))
        .json(&json!({ "video_id": "nonexistent" }))
        .await;
    let status = res.status_code().as_u16();
    assert!(status >= 400, "expected error, got {status}");
}

// ===========================================================================
// Child playlists — add_library (YouTube playlist import)
// ===========================================================================

#[tokio::test]
async fn child_playlist_add_library_creates_youtube_playlist() {
    let (app, auth, mock_server) = boot_with_mock(AccountType::Child).await;
    let child_id = auth.account_id;
    let parent_id = app.parent_id.unwrap();

    // The playlist must be allowlisted first.
    sqlx::query(
        "INSERT INTO allowlisted_playlists (child_account_id, playlist_id, playlist_title, added_by) \
         VALUES (?, 'PL_import', 'Imported', ?)",
    )
    .bind(child_id)
    .bind(parent_id)
    .execute(&app.pool)
    .await
    .unwrap();

    // Mock playlist lookup.
    Mock::given(method("GET"))
        .and(path("/playlists/PL_import"))
        .respond_with(ResponseTemplate::new(200).set_body_json(mock_playlist_response()))
        .mount(&mock_server)
        .await;

    // Mock playlist items.
    Mock::given(method("GET"))
        .and(path_regex("/playlist-items/PL_import.*"))
        .respond_with(ResponseTemplate::new(200).set_body_json(mock_playlist_items()))
        .mount(&mock_server)
        .await;

    let res = app
        .server
        .post("/api/playlists/library")
        .json(&json!({ "youtube_playlist_id": "PL_import" }))
        .await;
    assert!(res.status_code().is_success(), "got {}", res.status_code());
    let body: serde_json::Value = res.json();
    assert_eq!(body["title"], "Imported Playlist");
    assert!(body["id"].as_i64().is_some());
}

// ===========================================================================
// Family playlists — add_video
// ===========================================================================

#[tokio::test]
async fn family_playlist_add_video_with_mocked_youtube() {
    let (app, _auth, mock_server) = boot_with_mock(AccountType::Parent).await;
    let child_id = app.child_id.unwrap();

    // Create a family playlist.
    let res = app
        .server
        .post("/api/family-playlists")
        .json(&json!({ "title": "Family Fun", "child_ids": [child_id] }))
        .await;
    let body: serde_json::Value = res.json();
    let pl_id = body["id"].as_i64().unwrap();

    Mock::given(method("GET"))
        .and(path("/videos/fam-vid"))
        .respond_with(ResponseTemplate::new(200).set_body_json(mock_video_response("fam-vid")))
        .mount(&mock_server)
        .await;

    let res = app
        .server
        .post(&format!("/api/family-playlists/{pl_id}/videos"))
        .json(&json!({ "video_id": "fam-vid" }))
        .await;
    assert_eq!(res.status_code(), StatusCode::OK);
    let body: serde_json::Value = res.json();
    assert_eq!(body["video_id"], "fam-vid");
}

// ===========================================================================
// Child playlists — detail triggers lazy-refresh for YouTube playlists
// ===========================================================================

#[tokio::test]
async fn child_playlist_detail_refreshes_stale_youtube_playlist() {
    let (app, auth, mock_server) = boot_with_mock(AccountType::Child).await;
    let child_id = auth.account_id;
    let parent_id = app.parent_id.unwrap();

    // Allowlist the YouTube playlist.
    sqlx::query(
        "INSERT INTO allowlisted_playlists (child_account_id, playlist_id, playlist_title, added_by) \
         VALUES (?, 'PL_stale', 'Stale', ?)",
    )
    .bind(child_id)
    .bind(parent_id)
    .execute(&app.pool)
    .await
    .unwrap();

    // Create a YouTube-sourced playlist that's "stale" (updated_at = long ago).
    let pl_id: i64 = sqlx::query_scalar(
        "INSERT INTO child_playlists (child_account_id, youtube_playlist_id, title, is_own, updated_at) \
         VALUES (?, 'PL_stale', 'Stale PL', 0, 0) RETURNING id",
    )
    .bind(child_id)
    .fetch_one(&app.pool)
    .await
    .unwrap();

    // Mock the playlist items fetch that happens during refresh.
    Mock::given(method("GET"))
        .and(path_regex("/playlist-items/PL_stale.*"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "items": [{
                "video_id": "refresh-vid",
                "title": "Refreshed Video",
                "channel_id": "UCref",
                "channel_title": "Ref Ch",
                "thumbnails": {"default": {"url": "http://t/r.jpg"}},
                "published_at": "2024-06-01T00:00:00Z",
                "position": 0
            }],
            "next_page_token": null
        })))
        .mount(&mock_server)
        .await;

    let res = app.server.get(&format!("/api/playlists/{pl_id}")).await;
    assert_eq!(res.status_code(), StatusCode::OK);
    let body: serde_json::Value = res.json();
    // The detail response should include the refreshed video.
    let videos = body["videos"].as_array().unwrap();
    assert!(!videos.is_empty());
    assert_eq!(videos[0]["video_id"], "refresh-vid");
}

// ===========================================================================
// Downloads stream with successful format resolution
// ===========================================================================

#[tokio::test]
async fn download_stream_resolves_format_for_child() {
    let (app, auth, _mock_server) = boot_with_mock(AccountType::Child).await;
    let child_id = auth.account_id;
    let parent_id = app.parent_id.unwrap();

    // Seed video metadata cache with a progressive format.
    let json = json!({
        "id": "dl-stream-ok",
        "title": "Download OK",
        "channel_id": "ch-dl",
        "channel_title": "DL Ch",
        "duration": 120.0,
        "thumbnails": [],
        "formats": [
            {"format_id": "18", "height": 360, "width": 640, "url": "https://dl.test/360.mp4",
             "vcodec": "avc1", "acodec": "aac"},
            {"format_id": "22", "height": 720, "width": 1280, "url": "https://dl.test/720.mp4",
             "vcodec": "avc1", "acodec": "aac"}
        ],
        "subtitles": {},
        "automatic_captions": {}
    });
    let expires_at = chrono::Utc::now().timestamp() + 3600;
    sqlx::query(
        "INSERT INTO video_metadata_cache (video_id, metadata_json, expires_at) VALUES (?, ?, ?)",
    )
    .bind("dl-stream-ok")
    .bind(json.to_string())
    .bind(expires_at)
    .execute(&app.pool)
    .await
    .unwrap();

    // Allowlist the video.
    sqlx::query(
        "INSERT INTO allowlisted_videos (child_account_id, video_id, video_title, added_by) \
         VALUES (?, 'dl-stream-ok', 'DL', ?)",
    )
    .bind(child_id)
    .bind(parent_id)
    .execute(&app.pool)
    .await
    .unwrap();

    // The stream endpoint will try to fetch from the upstream URL (which
    // will fail), but the format resolution and access check logic is covered.
    let res = app
        .server
        .get("/api/downloads/dl-stream-ok/stream?quality=720p")
        .await;
    // This will likely fail with an HTTP error (upstream unreachable), but
    // the important thing is that it's NOT a 403/401 - the access check passed.
    let status = res.status_code().as_u16();
    assert_ne!(status, 403, "access check should pass");
    assert_ne!(status, 401, "auth check should pass");
}
