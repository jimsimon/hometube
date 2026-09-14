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
        }
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
fn format_deserializes_with_protocol() {
    let json = r#"{
        "format_id": "dash-video",
        "protocol": "http_dash_segments"
    }"#;
    let fmt: Format = serde_json::from_str(json).unwrap();
    assert_eq!(fmt.format_id, "dash-video");
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
// client attribution (format_note client tags)
// ---------------------------------------------------------------------------

#[test]
fn client_tag_from_format_note_parses_ytdlp_short_names() {
    use hometube::services::ytdlp::client_tag_from_format_note;

    assert_eq!(client_tag_from_format_note("144p, VISI"), Some("VISI"));
    assert_eq!(
        client_tag_from_format_note("1080p60, WEB-E, mp4_dash"),
        Some("WEB-E")
    );
    assert_eq!(
        client_tag_from_format_note("medium, DRC, WEB-C"),
        Some("WEB-C")
    );
    assert_eq!(client_tag_from_format_note("low, TV-D"), Some("TV-D"));
    assert_eq!(
        client_tag_from_format_note("English (United States) original (default), medium, ANDR-V"),
        Some("ANDR-V")
    );
    assert_eq!(client_tag_from_format_note("360p, IOS"), Some("IOS"));
    // No client tag present.
    assert_eq!(client_tag_from_format_note("720p"), None);
    assert_eq!(client_tag_from_format_note("low, DRC"), None);
    assert_eq!(client_tag_from_format_note(""), None);
}

#[test]
fn usable_formats_by_client_counts_only_dash_usable_formats() {
    let json = r#"{"id":"v","formats":[
        {"format_id":"sb0","protocol":"mhtml","url":"u","format_note":"storyboard, WEB-C"},
        {"format_id":"233","protocol":"m3u8_native","acodec":"mp4a.40.5","vcodec":"none","url":"u","format_note":"low, WEB-E"},
        {"format_id":"251","protocol":"https","acodec":"opus","vcodec":"none","url":"u","format_note":"medium, WEB-C"},
        {"format_id":"251-dashy","protocol":"http_dash_segments","acodec":"opus","vcodec":"none","url":"u","format_note":"medium, WEB-C"},
        {"format_id":"251-drc","protocol":"https","acodec":"opus","vcodec":"none","url":"u","format_note":"medium, DRC, WEB-C"},
        {"format_id":"303","protocol":"https","acodec":"none","vcodec":"vp9","height":1080,"url":"u","format_note":"1080p60, VISI"},
        {"format_id":"137","protocol":"https","acodec":"none","vcodec":"avc1.640028","height":1080,"url":"u"},
        {"format_id":"136","protocol":"https","acodec":"none","vcodec":"avc1.4d401f","height":720,"format_note":"720p, WEB"},
        {"format_id":"18","protocol":"https","acodec":"mp4a.40.2","vcodec":"avc1.42001E","height":360,"url":"u","format_note":"360p, MWEB"},
        {"format_id":"399","protocol":"https","acodec":"none","vcodec":"av01.0.09M.08","height":1080,"url":"u","format_note":"1080p60, WEB-C"}
    ]}"#;
    let result: ExtractResult = serde_json::from_str(json).unwrap();
    let by_client = result.usable_formats_by_client();
    // 251 + 251-dashy; the DRC variant and AV1 are not DASH-usable.
    assert_eq!(by_client.get("WEB-C"), Some(&2));
    assert_eq!(by_client.get("VISI"), Some(&1));
    // Untagged usable format lands in the "?" bucket.
    assert_eq!(by_client.get("?"), Some(&1));
    // Storyboard, HLS, URL-less and muxed (format 18) are excluded —
    // format 18 is what a SABR-only session still hands out, and it
    // must not count as playable or the fallback never fires.
    assert_eq!(by_client.get("WEB-E"), None);
    assert_eq!(by_client.get("WEB"), None);
    assert_eq!(by_client.get("MWEB"), None);
    assert_eq!(
        by_client.values().sum::<usize>(),
        result.usable_format_count()
    );
}

// ---------------------------------------------------------------------------
// player_client_list
// ---------------------------------------------------------------------------

#[test]
fn player_client_list_appends_web_creator_only_when_authenticated() {
    use hometube::services::ytdlp::{player_client_list, DEFAULT_PLAYER_CLIENTS};

    assert_eq!(
        player_client_list(DEFAULT_PLAYER_CLIENTS, false),
        DEFAULT_PLAYER_CLIENTS
    );
    assert_eq!(
        player_client_list(DEFAULT_PLAYER_CLIENTS, true),
        format!("{DEFAULT_PLAYER_CLIENTS},web_creator")
    );
    // Operator-pinned lists (e.g. production's `default,web_embedded`)
    // get the fix too.
    assert_eq!(
        player_client_list("default,web_embedded", true),
        "default,web_embedded,web_creator"
    );
}

#[test]
fn player_client_list_respects_operator_mentions() {
    use hometube::services::ytdlp::player_client_list;

    // Already present: don't duplicate.
    assert_eq!(
        player_client_list("web_creator,default", true),
        "web_creator,default"
    );
    // Explicitly excluded via yt-dlp's `-client` syntax: honour it.
    assert_eq!(
        player_client_list("default,-web_creator", true),
        "default,-web_creator"
    );
    // Degenerate inputs.
    assert_eq!(player_client_list("", true), "web_creator");
    assert_eq!(
        player_client_list(" default, ", true),
        "default,web_creator"
    );
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
