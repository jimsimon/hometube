//! Extended video route tests — covers captions with data, metadata via
//! channel allowlist, anonymous access rejection, and more edge cases.

mod common;

use axum::http::StatusCode;
use chrono::Utc;
use common::{boot_setup_complete, boot_with_parent_and_child};
use hometube::models::account::AccountType;

/// Seed `video_metadata_cache` with customisable JSON.
async fn seed_metadata_json(pool: &sqlx::SqlitePool, video_id: &str, json: &serde_json::Value) {
    let expires_at = Utc::now().timestamp() + 3600;
    sqlx::query(
        "INSERT INTO video_metadata_cache (video_id, metadata_json, expires_at) \
         VALUES (?, ?, ?) ON CONFLICT(video_id) DO UPDATE SET metadata_json = excluded.metadata_json, expires_at = excluded.expires_at",
    )
    .bind(video_id)
    .bind(json.to_string())
    .bind(expires_at)
    .execute(pool)
    .await
    .unwrap();
}

fn base_metadata(video_id: &str, channel_id: &str) -> serde_json::Value {
    serde_json::json!({
        "id": video_id,
        "title": "Test Video",
        "channel_id": channel_id,
        "channel_title": "Test Channel",
        "duration": 180.0,
        "thumbnails": [
            {"url": "http://thumb.example/maxres.jpg", "width": 1280, "height": 720}
        ],
        "thumbnail": "http://thumb.example/default.jpg",
        "formats": [
            {"format_id": "137", "height": 1080, "width": 1920, "url": "https://x/v1080"},
            {"format_id": "136", "height": 720, "width": 1280, "url": "https://x/v720"},
            {"format_id": "135", "height": 480, "width": 854, "url": "https://x/v480"},
            {"format_id": "251", "vcodec": "none", "acodec": "opus", "abr": 128.0, "url": "https://x/audio"}
        ],
        "subtitles": {
            "en": [{"ext": "vtt", "url": "https://subs.example/en.vtt", "name": "English"}],
            "es": [{"ext": "srv3", "url": "https://subs.example/es.srv3", "name": "Spanish"}]
        },
        "automatic_captions": {
            "fr": [{"ext": "vtt", "url": "https://subs.example/fr.vtt"}],
            "de": [{"ext": "srv1", "url": "https://subs.example/de.srv1"}]
        }
    })
}

// ---------------------------------------------------------------------------
// Captions
// ---------------------------------------------------------------------------

/// `list_captions` returns *only* user-uploaded subtitles. The
/// auto-translated languages from yt-dlp's `automatic_captions` map are
/// deliberately omitted: rendering them all as `<track>` elements
/// causes the browser to eagerly fetch every variant in parallel, which
/// instantly trips YouTube's caption rate limit (HTTP 429) and
/// cascades into the bot-check wall on the yt-dlp fallback path.
#[tokio::test]
async fn list_captions_returns_only_manual_tracks() {
    let (app, auth) = boot_with_parent_and_child(AccountType::Child).await;
    let child_id = auth.account_id;
    let parent_id = app.parent_id.unwrap();

    let json = base_metadata("vid-caps", "chan-1");
    seed_metadata_json(&app.pool, "vid-caps", &json).await;

    sqlx::query(
        "INSERT INTO allowlisted_videos (child_account_id, video_id, video_title, added_by) \
         VALUES (?, 'vid-caps', 'Title', ?)",
    )
    .bind(child_id)
    .bind(parent_id)
    .execute(&app.pool)
    .await
    .unwrap();

    let res = app.server.get("/api/videos/vid-caps/captions").await;
    assert_eq!(res.status_code(), StatusCode::OK);
    let body: serde_json::Value = res.json();
    let arr = body.as_array().unwrap();
    // 2 manual (en, es); the 2 auto entries (fr, de) must be hidden.
    assert_eq!(arr.len(), 2, "auto-captions must be filtered out: {body}");
    let auto_count = arr.iter().filter(|t| t["auto_generated"] == true).count();
    let manual_count = arr.iter().filter(|t| t["auto_generated"] == false).count();
    assert_eq!(manual_count, 2);
    assert_eq!(auto_count, 0, "no auto tracks should be exposed: {body}");
}

// ---------------------------------------------------------------------------
// Access via channel allowlist
// ---------------------------------------------------------------------------

#[tokio::test]
async fn metadata_allowed_via_channel_allowlist() {
    let (app, auth) = boot_with_parent_and_child(AccountType::Child).await;
    let child_id = auth.account_id;
    let parent_id = app.parent_id.unwrap();

    seed_metadata_json(&app.pool, "vid-chan", &base_metadata("vid-chan", "chan-A")).await;

    // Allowlist the channel, not the video directly.
    sqlx::query(
        "INSERT INTO allowlisted_channels (child_account_id, channel_id, channel_title, added_by) \
         VALUES (?, 'chan-A', 'Channel A', ?)",
    )
    .bind(child_id)
    .bind(parent_id)
    .execute(&app.pool)
    .await
    .unwrap();

    let res = app.server.get("/api/videos/vid-chan").await;
    assert_eq!(res.status_code(), StatusCode::OK);
    let body: serde_json::Value = res.json();
    assert_eq!(body["id"], "vid-chan");
}

#[tokio::test]
async fn metadata_denied_when_not_allowlisted() {
    let (app, _auth) = boot_with_parent_and_child(AccountType::Child).await;
    seed_metadata_json(
        &app.pool,
        "vid-denied",
        &base_metadata("vid-denied", "chan-Z"),
    )
    .await;

    let res = app.server.get("/api/videos/vid-denied").await;
    assert_eq!(res.status_code(), StatusCode::FORBIDDEN);
}

// ---------------------------------------------------------------------------
// Anonymous access
// ---------------------------------------------------------------------------

#[tokio::test]
async fn anonymous_metadata_is_401() {
    let (app, _auth) = boot_with_parent_and_child(AccountType::Child).await;
    seed_metadata_json(&app.pool, "vid-1", &base_metadata("vid-1", "chan-1")).await;

    let res = app.server.get("/api/videos/vid-1").clear_cookies().await;
    assert_eq!(res.status_code(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn anonymous_stream_is_401() {
    let (app, _auth) = boot_with_parent_and_child(AccountType::Child).await;
    seed_metadata_json(&app.pool, "vid-1", &base_metadata("vid-1", "chan-1")).await;

    let res = app
        .server
        .get("/api/videos/vid-1/stream")
        .clear_cookies()
        .await;
    assert_eq!(res.status_code(), StatusCode::UNAUTHORIZED);
}

// ---------------------------------------------------------------------------
// Stream endpoint
// ---------------------------------------------------------------------------

#[tokio::test]
async fn stream_for_parent_returns_all_formats_without_cap() {
    let (app, _auth) = boot_with_parent_and_child(AccountType::Parent).await;
    seed_metadata_json(&app.pool, "vid-2", &base_metadata("vid-2", "chan-1")).await;

    let res = app.server.get("/api/videos/vid-2/stream").await;
    assert_eq!(res.status_code(), StatusCode::OK);
    let body: serde_json::Value = res.json();
    let formats = body["formats"].as_array().unwrap();
    // Parent gets all 4 formats (1080, 720, 480, audio).
    assert_eq!(formats.len(), 4);
}

#[tokio::test]
async fn stream_with_480p_cap_filters_correctly() {
    let (app, auth) = boot_with_parent_and_child(AccountType::Child).await;
    let child_id = auth.account_id;
    let parent_id = app.parent_id.unwrap();

    seed_metadata_json(&app.pool, "vid-3", &base_metadata("vid-3", "chan-1")).await;
    sqlx::query(
        "INSERT INTO allowlisted_videos (child_account_id, video_id, video_title, added_by) \
         VALUES (?, 'vid-3', 'Title', ?)",
    )
    .bind(child_id)
    .bind(parent_id)
    .execute(&app.pool)
    .await
    .unwrap();

    // Set 480p cap.
    sqlx::query(
        "INSERT INTO child_settings (child_account_id, max_quality) \
         VALUES (?, '480p') ON CONFLICT(child_account_id) DO UPDATE SET max_quality = '480p'",
    )
    .bind(child_id)
    .execute(&app.pool)
    .await
    .unwrap();

    let res = app.server.get("/api/videos/vid-3/stream").await;
    assert_eq!(res.status_code(), StatusCode::OK);
    let body: serde_json::Value = res.json();
    let formats = body["formats"].as_array().unwrap();
    // Only 480 + audio survive.
    for f in formats {
        if let Some(h) = f["height"].as_i64() {
            assert!(h <= 480, "got height {h} despite 480p cap");
        }
    }
}

#[tokio::test]
async fn stream_with_no_quality_setting_returns_all() {
    let (app, auth) = boot_with_parent_and_child(AccountType::Child).await;
    let child_id = auth.account_id;
    let parent_id = app.parent_id.unwrap();

    seed_metadata_json(&app.pool, "vid-4", &base_metadata("vid-4", "chan-1")).await;
    sqlx::query(
        "INSERT INTO allowlisted_videos (child_account_id, video_id, video_title, added_by) \
         VALUES (?, 'vid-4', 'Title', ?)",
    )
    .bind(child_id)
    .bind(parent_id)
    .execute(&app.pool)
    .await
    .unwrap();

    // No child_settings row → no cap applied.
    let res = app.server.get("/api/videos/vid-4/stream").await;
    assert_eq!(res.status_code(), StatusCode::OK);
    let body: serde_json::Value = res.json();
    let formats = body["formats"].as_array().unwrap();
    assert_eq!(formats.len(), 4);
}

// ---------------------------------------------------------------------------
// Manifest endpoint
// ---------------------------------------------------------------------------

#[tokio::test]
async fn manifest_returns_404_without_usable_formats() {
    let (app, _auth) = boot_with_parent_and_child(AccountType::Parent).await;
    // Seed metadata without any usable formats.
    let json = serde_json::json!({
        "id": "vid-no-m",
        "title": "No Manifest",
        "channel_id": "ch",
        "formats": [{"format_id": "137", "height": 1080, "url": "https://x/v"}],
        "subtitles": {},
        "automatic_captions": {}
    });
    seed_metadata_json(&app.pool, "vid-no-m", &json).await;

    let res = app
        .server
        .get("/api/videos/vid-no-m/stream/manifest.mpd")
        .await;
    assert_eq!(res.status_code(), StatusCode::NOT_FOUND);
}

// ---------------------------------------------------------------------------
// Preview endpoint (parent only)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn preview_video_as_child_is_forbidden() {
    let (app, _auth) = boot_with_parent_and_child(AccountType::Child).await;
    seed_metadata_json(&app.pool, "vid-prev", &base_metadata("vid-prev", "chan-1")).await;

    let res = app.server.get("/api/preview/video/vid-prev").await;
    assert_eq!(res.status_code(), StatusCode::FORBIDDEN);
}

// ---------------------------------------------------------------------------
// Stream with 1080p cap
// ---------------------------------------------------------------------------

#[tokio::test]
async fn stream_with_1080p_cap_keeps_everything() {
    let (app, auth) = boot_with_parent_and_child(AccountType::Child).await;
    let child_id = auth.account_id;
    let parent_id = app.parent_id.unwrap();

    seed_metadata_json(&app.pool, "vid-5", &base_metadata("vid-5", "chan-1")).await;
    sqlx::query(
        "INSERT INTO allowlisted_videos (child_account_id, video_id, video_title, added_by) \
         VALUES (?, 'vid-5', 'Title', ?)",
    )
    .bind(child_id)
    .bind(parent_id)
    .execute(&app.pool)
    .await
    .unwrap();

    sqlx::query(
        "INSERT INTO child_settings (child_account_id, max_quality) \
         VALUES (?, '1080p') ON CONFLICT(child_account_id) DO UPDATE SET max_quality = '1080p'",
    )
    .bind(child_id)
    .execute(&app.pool)
    .await
    .unwrap();

    let res = app.server.get("/api/videos/vid-5/stream").await;
    assert_eq!(res.status_code(), StatusCode::OK);
    let body: serde_json::Value = res.json();
    let formats = body["formats"].as_array().unwrap();
    assert_eq!(formats.len(), 4);
}

// ---------------------------------------------------------------------------
// Thumbnail proxy
// ---------------------------------------------------------------------------

#[tokio::test]
async fn thumbnail_proxy_404_when_no_thumbnails() {
    let (app, _auth) = boot_setup_complete(AccountType::Parent).await;
    let json = serde_json::json!({
        "id": "vid-nothumb",
        "title": "No Thumbs",
        "channel_id": "ch",
        "formats": [],
        "thumbnails": [],
        "subtitles": {},
        "automatic_captions": {}
    });
    seed_metadata_json(&app.pool, "vid-nothumb", &json).await;

    let res = app.server.get("/api/proxy/thumbnail/vid-nothumb").await;
    assert_eq!(res.status_code(), StatusCode::NOT_FOUND);
}
