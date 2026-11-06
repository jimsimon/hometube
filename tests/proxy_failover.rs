//! Tests for the upstream-403 failover in `/api/proxy/format`.
//!
//! YouTube hands out one `videoplayback` URL per player client for the
//! same file. Some of those URLs stop working part-way through the file
//! (TV clients on a logged-in session refuse ranges past roughly the
//! first minute of media with 403). When the format the player was
//! given answers 403, the proxy must retry against another client's
//! copy of the *same* file and remember which copy worked.
//!
//! Each test stands up its own wiremock "CDN" with two paths: `/tv`
//! (the URL yt-dlp listed first) and `/web` (the alternate).

mod common;

use axum::http::{header, HeaderValue, StatusCode};
use common::boot_with_parent_and_child;
use hometube::models::account::AccountType;
use hometube::services::dash::{build_format_proxy_url, ensure_proxy_secret};
use serde_json::json;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const TOTAL: u64 = 4096;
const LMT: &str = "1758150000000000";

/// Mount `/tv` answering 403 and `/web` answering the requested range
/// with `web_bytes`.
async fn mock_cdn(web_bytes: &[u8]) -> MockServer {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/tv"))
        .respond_with(
            ResponseTemplate::new(403)
                .insert_header("Content-Type", "text/plain")
                .set_body_bytes(b"forbidden".to_vec()),
        )
        .mount(&server)
        .await;
    let end = web_bytes.len() as u64 - 1;
    Mock::given(method("GET"))
        .and(path("/web"))
        .respond_with(
            ResponseTemplate::new(206)
                .insert_header("Content-Type", "video/webm")
                .insert_header("Content-Range", format!("bytes 0-{end}/{TOTAL}").as_str())
                .insert_header("Accept-Ranges", "bytes")
                .set_body_bytes(web_bytes.to_vec()),
        )
        .mount(&server)
        .await;
    server
}

fn video_format(format_id: &str, note: &str, url: &str) -> serde_json::Value {
    json!({
        "format_id": format_id,
        "format_note": note,
        "ext": "webm",
        "url": url,
        "protocol": "http_dash_segments",
        "filesize": TOTAL,
        "vcodec": "vp9",
        "acodec": "none",
        "height": 720,
        "width": 1280,
        "tbr": 2500.0
    })
}

/// Seed the metadata cache with an extraction whose format list is
/// `formats`, stamped as produced by the current extractor config.
async fn seed_metadata(pool: &sqlx::SqlitePool, video_id: &str, formats: Vec<serde_json::Value>) {
    let meta = json!({
        "id": video_id,
        "title": "Test Video",
        "formats": formats,
        "thumbnails": []
    });
    let now = chrono::Utc::now().timestamp();
    sqlx::query(
        "INSERT OR REPLACE INTO video_metadata_cache (video_id, metadata_json, cached_at, expires_at) \
         VALUES (?, ?, ?, ?)",
    )
    .bind(video_id)
    .bind(meta.to_string())
    .bind(now)
    .bind(now + 3600)
    .execute(pool)
    .await
    .unwrap();
    common::mark_metadata_cache_current(pool).await;
}

/// Number of requests the mock CDN received for `p`.
async fn hits(server: &MockServer, p: &str) -> usize {
    server
        .received_requests()
        .await
        .unwrap()
        .iter()
        .filter(|r| r.url.path() == p)
        .count()
}

async fn proxy_get(
    app: &common::TestApp,
    video_id: &str,
    format_id: &str,
    range: &str,
) -> axum_test::TestResponse {
    let secret = ensure_proxy_secret(&app.pool).await.unwrap();
    let url = build_format_proxy_url(&secret, video_id, format_id);
    app.server
        .get(&url)
        .add_header(header::RANGE, HeaderValue::from_str(range).unwrap())
        .await
}

#[tokio::test]
async fn forbidden_primary_fails_over_to_other_client_copy_of_same_file() {
    let (app, _auth) = boot_with_parent_and_child(AccountType::Child).await;
    let web_bytes: Vec<u8> = (0..1024u32).map(|i| (i % 251) as u8).collect();
    let cdn = mock_cdn(&web_bytes).await;

    let video_id = "failover-vid";
    seed_metadata(
        &app.pool,
        video_id,
        vec![
            video_format(
                "247-dashy-0",
                "720p, TV-S",
                &format!("{}/tv?lmt={LMT}", cdn.uri()),
            ),
            video_format(
                "247-dashy-1",
                "720p, WEB-E",
                &format!("{}/web?lmt={LMT}", cdn.uri()),
            ),
        ],
    )
    .await;

    let res = proxy_get(&app, video_id, "247-dashy-0", "bytes=0-1023").await;

    assert_eq!(res.status_code(), StatusCode::PARTIAL_CONTENT);
    assert_eq!(res.as_bytes().as_ref(), web_bytes.as_slice());
    assert_eq!(
        res.header("content-range").to_str().unwrap(),
        format!("bytes 0-1023/{TOTAL}")
    );
    assert_eq!(hits(&cdn, "/tv").await, 1, "primary URL tried once");
    assert_eq!(hits(&cdn, "/web").await, 1, "alternate served the bytes");

    // The failing URL is remembered: the next segment goes straight to
    // the copy that worked.
    let res = proxy_get(&app, video_id, "247-dashy-0", "bytes=1024-2047").await;
    assert_eq!(res.status_code(), StatusCode::PARTIAL_CONTENT);
    assert_eq!(res.as_bytes().as_ref(), web_bytes.as_slice());
    assert_eq!(hits(&cdn, "/tv").await, 1, "dead URL not retried");
    assert_eq!(hits(&cdn, "/web").await, 2);
}

#[tokio::test]
async fn alternate_from_a_different_encode_is_not_used() {
    let (app, _auth) = boot_with_parent_and_child(AccountType::Child).await;
    let cdn = mock_cdn(&[0xAB; 1024]).await;

    let video_id = "other-encode-vid";
    seed_metadata(
        &app.pool,
        video_id,
        vec![
            video_format(
                "247-dashy-0",
                "720p, TV-S",
                &format!("{}/tv?lmt={LMT}", cdn.uri()),
            ),
            // Same itag, but a different `lmt=`: a different file whose
            // byte offsets would not line up with the player's index.
            video_format(
                "247-dashy-1",
                "720p, WEB-E",
                &format!("{}/web?lmt=1700000000000000", cdn.uri()),
            ),
        ],
    )
    .await;

    let res = proxy_get(&app, video_id, "247-dashy-0", "bytes=0-1023").await;

    assert_eq!(res.status_code(), StatusCode::FORBIDDEN);
    assert_eq!(hits(&cdn, "/tv").await, 1);
    assert_eq!(
        hits(&cdn, "/web").await,
        0,
        "different file must not be substituted"
    );
}

#[tokio::test]
async fn alternate_with_a_different_itag_is_not_used() {
    let (app, _auth) = boot_with_parent_and_child(AccountType::Child).await;
    let cdn = mock_cdn(&[0xCD; 1024]).await;

    let video_id = "other-itag-vid";
    seed_metadata(
        &app.pool,
        video_id,
        vec![
            video_format(
                "247-dashy-0",
                "720p, TV-S",
                &format!("{}/tv?lmt={LMT}", cdn.uri()),
            ),
            // Same fingerprint by coincidence, but a different itag is a
            // different encode by definition.
            video_format(
                "248-dashy-1",
                "1080p, WEB-E",
                &format!("{}/web?lmt={LMT}", cdn.uri()),
            ),
        ],
    )
    .await;

    let res = proxy_get(&app, video_id, "247-dashy-0", "bytes=0-1023").await;

    assert_eq!(res.status_code(), StatusCode::FORBIDDEN);
    assert_eq!(hits(&cdn, "/web").await, 0);
}

#[tokio::test]
async fn non_forbidden_upstream_errors_pass_through_without_failover() {
    let (app, _auth) = boot_with_parent_and_child(AccountType::Child).await;
    let cdn = mock_cdn(&[0xEF; 1024]).await;
    // A third path answering 416: the file's own verdict on the range.
    Mock::given(method("GET"))
        .and(path("/tv-416"))
        .respond_with(ResponseTemplate::new(416))
        .mount(&cdn)
        .await;

    let video_id = "range-vid";
    seed_metadata(
        &app.pool,
        video_id,
        vec![
            video_format(
                "247-dashy-0",
                "720p, TV-S",
                &format!("{}/tv-416?lmt={LMT}", cdn.uri()),
            ),
            video_format(
                "247-dashy-1",
                "720p, WEB-E",
                &format!("{}/web?lmt={LMT}", cdn.uri()),
            ),
        ],
    )
    .await;

    let res = proxy_get(&app, video_id, "247-dashy-0", "bytes=9000-9999").await;

    assert_eq!(res.status_code(), StatusCode::RANGE_NOT_SATISFIABLE);
    assert_eq!(hits(&cdn, "/web").await, 0, "only 403 triggers failover");
}

#[tokio::test]
async fn forbidden_everywhere_returns_forbidden() {
    let (app, _auth) = boot_with_parent_and_child(AccountType::Child).await;
    let cdn = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(403))
        .mount(&cdn)
        .await;

    let video_id = "all-forbidden-vid";
    seed_metadata(
        &app.pool,
        video_id,
        vec![
            video_format(
                "247-dashy-0",
                "720p, TV-S",
                &format!("{}/a?lmt={LMT}", cdn.uri()),
            ),
            video_format(
                "247-dashy-1",
                "720p, WEB-E",
                &format!("{}/b?lmt={LMT}", cdn.uri()),
            ),
        ],
    )
    .await;

    let res = proxy_get(&app, video_id, "247-dashy-0", "bytes=0-1023").await;

    assert_eq!(res.status_code(), StatusCode::FORBIDDEN);
    assert_eq!(hits(&cdn, "/a").await, 1);
    assert_eq!(hits(&cdn, "/b").await, 1);
}
