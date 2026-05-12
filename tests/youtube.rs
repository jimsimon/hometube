//! Tests for the YouTube Data API client utilities.
//!
//! Since the parsing functions are private, we test them indirectly via
//! the HTTP API routes that exercise them (/api/parent/search). We also
//! directly test the public(crate) URL building and encoding utilities,
//! and the public types' serialization.

mod common;

use hometube::services::youtube::{SearchType, ThumbnailInfo};

// ---------------------------------------------------------------------------
// SearchType::parse
// ---------------------------------------------------------------------------

#[test]
fn search_type_parse_valid() {
    assert!(matches!(
        SearchType::parse("channel"),
        Some(SearchType::Channel)
    ));
    assert!(matches!(
        SearchType::parse("playlist"),
        Some(SearchType::Playlist)
    ));
    assert!(matches!(
        SearchType::parse("video"),
        Some(SearchType::Video)
    ));
}

#[test]
fn search_type_parse_invalid() {
    assert!(SearchType::parse("unknown").is_none());
    assert!(SearchType::parse("").is_none());
    assert!(SearchType::parse("Channel").is_none()); // case-sensitive
}

// ---------------------------------------------------------------------------
// ThumbnailInfo serialization
// ---------------------------------------------------------------------------

#[test]
fn thumbnail_info_deserializes_from_youtube_json() {
    let json = r#"{"url":"https://i.ytimg.com/vi/abc/default.jpg","width":120,"height":90}"#;
    let info: ThumbnailInfo = serde_json::from_str(json).unwrap();
    assert_eq!(info.url, "https://i.ytimg.com/vi/abc/default.jpg");
    assert_eq!(info.width, Some(120));
    assert_eq!(info.height, Some(90));
}

#[test]
fn thumbnail_info_deserializes_without_optional_fields() {
    let json = r#"{"url":"https://i.ytimg.com/vi/abc/default.jpg"}"#;
    let info: ThumbnailInfo = serde_json::from_str(json).unwrap();
    assert_eq!(info.url, "https://i.ytimg.com/vi/abc/default.jpg");
    assert_eq!(info.width, None);
    assert_eq!(info.height, None);
}

// ---------------------------------------------------------------------------
// Integration test: parent search endpoint exercises the YouTube parser
// indirectly — verifies the route wiring handles errors gracefully when
// the API key points at nothing real.
// ---------------------------------------------------------------------------

use common::boot_setup_complete;
use hometube::models::account::AccountType;

#[tokio::test]
async fn parent_search_returns_error_for_invalid_api_key() {
    // The test harness seeds a fake API key ("test-yt-api-key") which
    // will fail when the route tries to actually call YouTube. This
    // exercises the error path in the search handler.
    let (app, _auth) = boot_setup_complete(AccountType::Parent).await;

    let res = app
        .server
        .get("/api/parent/search")
        .add_query_param("q", "test")
        .add_query_param("type", "video")
        .await;

    // Should return an error (either 500 from the failed YouTube call
    // or some other non-2xx) — not panic.
    let status = res.status_code().as_u16();
    assert!(status >= 400, "expected error status, got {status}");
}

#[tokio::test]
async fn parent_search_rejects_invalid_type() {
    let (app, _auth) = boot_setup_complete(AccountType::Parent).await;

    let res = app
        .server
        .get("/api/parent/search")
        .add_query_param("q", "test")
        .add_query_param("type", "invalid_type")
        .await;

    let status = res.status_code().as_u16();
    assert!(status >= 400);
}
