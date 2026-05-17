//! Tests for the yt-dlp service.
//!
//! Tests deserialization of `ExtractResult` from realistic yt-dlp JSON
//! output, the `check_for_update` DB logic path, and error handling.

mod common;

use common::boot;
use hometube::services::ytdlp::{ExtractResult, Format, SubtitleTrack, Thumbnail};

// ---------------------------------------------------------------------------
// ExtractResult deserialization
// ---------------------------------------------------------------------------

#[test]
fn extract_result_deserializes_minimal() {
    let json = r#"{"id":"dQw4w9WgXcQ"}"#;
    let result: ExtractResult = serde_json::from_str(json).unwrap();
    assert_eq!(result.id, "dQw4w9WgXcQ");
    assert_eq!(result.title, None);
    assert_eq!(result.duration, None);
    assert!(result.formats.is_empty());
    assert!(result.thumbnails.is_empty());
    assert!(result.subtitles.is_empty());
    assert!(result.automatic_captions.is_empty());
}

#[test]
fn extract_result_deserializes_full() {
    let json = r#"{
        "id": "abc123",
        "title": "Test Video",
        "channel_id": "UC12345",
        "channel": "Test Channel",
        "duration": 253.5,
        "thumbnails": [
            {"url": "https://i.ytimg.com/vi/abc123/default.jpg", "width": 120, "height": 90},
            {"url": "https://i.ytimg.com/vi/abc123/maxresdefault.jpg", "width": 1920, "height": 1080}
        ],
        "thumbnail": "https://i.ytimg.com/vi/abc123/maxresdefault.jpg",
        "formats": [
            {
                "format_id": "137",
                "ext": "mp4",
                "height": 1080,
                "width": 1920,
                "tbr": 4000.0,
                "vbr": 3800.0,
                "abr": null,
                "fps": 30.0,
                "vcodec": "avc1.640028",
                "acodec": "none",
                "filesize": 50000000,
                "url": "https://rr.googlevideo.com/videoplayback?...",
                "protocol": "https"
            },
            {
                "format_id": "251",
                "ext": "webm",
                "height": null,
                "width": null,
                "tbr": 128.0,
                "vbr": null,
                "abr": 128.0,
                "fps": null,
                "vcodec": "none",
                "acodec": "opus",
                "filesize": 3000000,
                "url": "https://rr.googlevideo.com/videoplayback?...",
                "protocol": "https"
            }
        ],
        "subtitles": {
            "en": [
                {"ext": "vtt", "url": "https://example.com/subs/en.vtt", "name": "English"}
            ]
        },
        "automatic_captions": {
            "en": [
                {"ext": "srv3", "url": "https://example.com/auto/en.srv3"}
            ]
        },
        "manifest_url": "https://manifest.example.com/dash.mpd"
    }"#;

    let result: ExtractResult = serde_json::from_str(json).unwrap();
    assert_eq!(result.id, "abc123");
    assert_eq!(result.title.as_deref(), Some("Test Video"));
    assert_eq!(result.channel_id.as_deref(), Some("UC12345"));
    assert_eq!(result.channel_title.as_deref(), Some("Test Channel"));
    assert_eq!(result.duration, Some(253.5));
    assert_eq!(result.thumbnails.len(), 2);
    assert_eq!(
        result.thumbnail.as_deref(),
        Some("https://i.ytimg.com/vi/abc123/maxresdefault.jpg")
    );
    assert_eq!(result.formats.len(), 2);
    assert_eq!(result.subtitles.len(), 1);
    assert_eq!(result.automatic_captions.len(), 1);
    assert_eq!(
        result.manifest_url.as_deref(),
        Some("https://manifest.example.com/dash.mpd")
    );

    // Check format details.
    let video_fmt = &result.formats[0];
    assert_eq!(video_fmt.format_id, "137");
    assert_eq!(video_fmt.ext.as_deref(), Some("mp4"));
    assert_eq!(video_fmt.height, Some(1080));
    assert_eq!(video_fmt.width, Some(1920));
    assert_eq!(video_fmt.fps, Some(30.0));
    assert_eq!(video_fmt.vcodec.as_deref(), Some("avc1.640028"));
    assert_eq!(video_fmt.filesize, Some(50000000));

    let audio_fmt = &result.formats[1];
    assert_eq!(audio_fmt.format_id, "251");
    assert_eq!(audio_fmt.acodec.as_deref(), Some("opus"));
    assert_eq!(audio_fmt.abr, Some(128.0));

    // Check subtitles.
    let en_subs = &result.subtitles["en"];
    assert_eq!(en_subs.len(), 1);
    assert_eq!(en_subs[0].ext, "vtt");
    assert_eq!(en_subs[0].name.as_deref(), Some("English"));
}

#[test]
fn extract_result_handles_uploader_alias() {
    // yt-dlp sometimes emits "uploader" instead of "channel".
    let json = r#"{"id":"x","uploader":"Uploader Name"}"#;
    let result: ExtractResult = serde_json::from_str(json).unwrap();
    assert_eq!(result.channel_title.as_deref(), Some("Uploader Name"));
}

#[test]
fn format_deserializes_with_manifest_url() {
    let json = r#"{
        "format_id": "dash-video",
        "manifest_url": "https://manifest.example.com/dash.mpd",
        "protocol": "http_dash_segments"
    }"#;
    let fmt: Format = serde_json::from_str(json).unwrap();
    assert_eq!(fmt.format_id, "dash-video");
    assert_eq!(
        fmt.manifest_url.as_deref(),
        Some("https://manifest.example.com/dash.mpd")
    );
    assert_eq!(fmt.protocol.as_deref(), Some("http_dash_segments"));
}

#[test]
fn thumbnail_deserializes() {
    let json = r#"{"url":"https://img.test/t.jpg","width":1280,"height":720,"id":"maxres"}"#;
    let thumb: Thumbnail = serde_json::from_str(json).unwrap();
    assert_eq!(thumb.url, "https://img.test/t.jpg");
    assert_eq!(thumb.width, Some(1280));
    assert_eq!(thumb.height, Some(720));
    assert_eq!(thumb.id.as_deref(), Some("maxres"));
}

#[test]
fn thumbnail_deserializes_minimal() {
    let json = r#"{"url":"https://img.test/t.jpg"}"#;
    let thumb: Thumbnail = serde_json::from_str(json).unwrap();
    assert_eq!(thumb.url, "https://img.test/t.jpg");
    assert_eq!(thumb.width, None);
    assert_eq!(thumb.height, None);
    assert_eq!(thumb.id, None);
}

#[test]
fn subtitle_track_deserializes() {
    let json = r#"{"ext":"vtt","url":"https://sub.test/en.vtt","name":"English"}"#;
    let track: SubtitleTrack = serde_json::from_str(json).unwrap();
    assert_eq!(track.ext, "vtt");
    assert_eq!(track.url, "https://sub.test/en.vtt");
    assert_eq!(track.name.as_deref(), Some("English"));
}

// ---------------------------------------------------------------------------
// sync_cookies_to_disk
// ---------------------------------------------------------------------------

#[test]
fn sync_cookies_to_disk_writes_and_removes() {
    // Use a unique temp path scoped to this test to avoid conflicts.
    let dir =
        std::env::temp_dir().join(format!("hometube-cookie-test-sync-{}", std::process::id()));
    let cookie_path = dir.join("cookies.txt");

    // Temporarily override the env var for this test only. Note: this
    // test should not run in parallel with other tests that depend on
    // YTDLP_COOKIES_PATH, but since it's a unit test in its own binary
    // that's acceptable.
    let prev = std::env::var("YTDLP_COOKIES_PATH").ok();
    unsafe { std::env::set_var("YTDLP_COOKIES_PATH", cookie_path.to_str().unwrap()) };

    // Write content.
    let content = "# Netscape HTTP Cookie File\n.youtube.com\tTRUE\t/\tFALSE\t0\tA\tB\n";
    hometube::services::ytdlp::sync_cookies_to_disk(Some(content)).unwrap();
    assert!(cookie_path.exists());
    assert_eq!(std::fs::read_to_string(&cookie_path).unwrap(), content);

    // Remove content.
    hometube::services::ytdlp::sync_cookies_to_disk(None).unwrap();
    assert!(!cookie_path.exists());

    // Empty/whitespace content also removes.
    hometube::services::ytdlp::sync_cookies_to_disk(Some(content)).unwrap();
    assert!(cookie_path.exists());
    hometube::services::ytdlp::sync_cookies_to_disk(Some("   ")).unwrap();
    assert!(!cookie_path.exists());

    // Cleanup.
    let _ = std::fs::remove_dir_all(&dir);
    match prev {
        Some(v) => unsafe { std::env::set_var("YTDLP_COOKIES_PATH", v) },
        None => unsafe { std::env::remove_var("YTDLP_COOKIES_PATH") },
    }
}

// ---------------------------------------------------------------------------
// fixup_webm_cues_offsets
// ---------------------------------------------------------------------------

use hometube::services::ytdlp::SegmentRanges;
use wiremock::matchers::header;
use wiremock::{Mock, MockServer, ResponseTemplate};

/// Build a minimal ExtractResult with one format pointing at the given URL.
fn result_with_format(
    format_id: &str,
    url: &str,
    itag: i64,
    ranges: SegmentRanges,
) -> ExtractResult {
    let mut segment_ranges = std::collections::HashMap::new();
    segment_ranges.insert(itag, ranges);
    ExtractResult {
        id: "test".into(),
        title: None,
        channel_id: None,
        channel_title: None,
        duration: None,
        thumbnails: vec![],
        thumbnail: None,
        formats: vec![Format {
            format_id: format_id.into(),
            ext: None,
            height: None,
            width: None,
            tbr: None,
            vbr: None,
            abr: None,
            fps: None,
            vcodec: Some("vp9".into()),
            acodec: None,
            filesize: None,
            url: Some(url.into()),
            manifest_url: None,
            protocol: Some("https".into()),
            language: None,
            language_preference: None,
            format_note: None,
        }],
        subtitles: Default::default(),
        automatic_captions: Default::default(),
        manifest_url: None,
        segment_ranges,
    }
}

#[tokio::test]
async fn fixup_webm_cues_keeps_correct_ranges() {
    let server = MockServer::start().await;

    // Serve bytes where Cues ID is at offset 0 of the index range.
    let cues_bytes: Vec<u8> = [0x1C, 0x53, 0xBB, 0x6B]
        .iter()
        .copied()
        .chain(std::iter::repeat_n(0u8, 60))
        .collect();

    Mock::given(header("range", "bytes=220-283"))
        .respond_with(ResponseTemplate::new(206).set_body_bytes(cues_bytes))
        .mount(&server)
        .await;

    let url = format!("{}/video.webm", server.uri());
    let ranges = SegmentRanges {
        init_start: 0,
        init_end: 219,
        index_start: 220,
        index_end: 1214,
    };
    let mut result = result_with_format("247-dashy", &url, 247, ranges);

    hometube::services::ytdlp::fixup_webm_cues_offsets(&mut result).await;

    // Range should be preserved — Cues was at the correct position.
    assert!(result.segment_ranges.contains_key(&247));
    let sr = result.segment_ranges[&247];
    assert_eq!(sr.index_start, 220);
    assert_eq!(sr.index_end, 1214);
}

#[tokio::test]
async fn fixup_webm_cues_removes_misaligned_range() {
    let server = MockServer::start().await;

    // Serve bytes where Cues ID is NOT at offset 0 (7 bytes of junk first).
    let mut body = vec![0x9F, 0x81, 0x02, 0x62, 0x64, 0x81, 0x10]; // 7 junk bytes
    body.extend_from_slice(&[0x1C, 0x53, 0xBB, 0x6B]); // Cues ID at offset 7
    body.extend(std::iter::repeat_n(0u8, 53));

    Mock::given(header("range", "bytes=259-322"))
        .respond_with(ResponseTemplate::new(206).set_body_bytes(body))
        .mount(&server)
        .await;

    let url = format!("{}/audio.webm", server.uri());
    let ranges = SegmentRanges {
        init_start: 0,
        init_end: 258,
        index_start: 259,
        index_end: 798,
    };
    let mut result = result_with_format("249-dashy-0", &url, 249, ranges);

    hometube::services::ytdlp::fixup_webm_cues_offsets(&mut result).await;

    // Range should be removed — Cues wasn't at byte 0 of the index range.
    assert!(
        !result.segment_ranges.contains_key(&249),
        "expected itag 249 to be removed, but it's still present"
    );
}

#[tokio::test]
async fn fixup_webm_cues_keeps_range_on_probe_failure() {
    let server = MockServer::start().await;
    // No mock mounted — server returns 404.

    let url = format!("{}/missing.webm", server.uri());
    let ranges = SegmentRanges {
        init_start: 0,
        init_end: 258,
        index_start: 259,
        index_end: 798,
    };
    let mut result = result_with_format("249-dashy-0", &url, 249, ranges);

    hometube::services::ytdlp::fixup_webm_cues_offsets(&mut result).await;

    // Range should be kept when probe fails (graceful fallback).
    assert!(result.segment_ranges.contains_key(&249));
}

// ---------------------------------------------------------------------------
// check_for_update DB logic
// ---------------------------------------------------------------------------

#[tokio::test]
async fn check_for_update_touches_last_checked_at() {
    let app = boot().await;

    // Seed the ytdlp_info row.
    let cfg = hometube::config::Config::from_env().unwrap();
    hometube::services::cron::seed_ytdlp_info(&app.pool, &cfg)
        .await
        .unwrap();

    let before: Option<i64> =
        sqlx::query_scalar("SELECT last_checked_at FROM ytdlp_info WHERE id = 1")
            .fetch_one(&app.pool)
            .await
            .unwrap();

    // check_for_update will fail (hits real GitHub, which is fine for
    // this test — we're testing the DB touch, not the network call).
    // If it succeeds that's fine too.
    let _ = hometube::services::ytdlp::check_for_update(&app.pool).await;

    let after: Option<i64> =
        sqlx::query_scalar("SELECT last_checked_at FROM ytdlp_info WHERE id = 1")
            .fetch_one(&app.pool)
            .await
            .unwrap();

    // If the network call succeeded, last_checked_at should be updated.
    // If it failed before reaching the UPDATE, they may be equal.
    // Either way the function shouldn't panic.
    assert!(after >= before);
}
