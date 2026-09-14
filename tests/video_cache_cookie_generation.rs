//! The metadata cache must not persist an extraction that overlapped a
//! cookie change.
//!
//! Kept in its own test binary: the test bumps the process-wide cookie
//! generation, which would make any extraction in flight in a sibling
//! test decline to cache and fail that test's assertions.

mod common;

use std::os::unix::fs::PermissionsExt;
use std::sync::Arc;
use std::time::Duration;

use common::boot;
use hometube::config::Config;
use hometube::services::video_cache::VideoCache;
use hometube::services::ytdlp;

/// Minimal yt-dlp JSON with one format whose URL expires far in the future.
fn metadata(video_id: &str, media_url: &str) -> serde_json::Value {
    serde_json::json!({
        "id": video_id,
        "title": "Cookie generation test",
        "formats": [{
            "format_id": "251",
            "ext": "webm",
            "url": media_url,
            "protocol": "https",
            "filesize": 4096,
            "acodec": "opus",
            "vcodec": "none"
        }],
        "thumbnails": []
    })
}

/// Write an executable shell script standing in for the yt-dlp binary.
fn write_script(path: &std::path::Path, script: &str) {
    std::fs::write(path, script).expect("write fake yt-dlp");
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755))
        .expect("chmod fake yt-dlp");
}

/// A cookie upload updates the `app_config` value (input to the
/// fingerprint) and `cookies.txt` (input to yt-dlp) one after the other,
/// so an extraction overlapping the upload may pair a fingerprint from
/// one jar with output from the other. The upload route brackets itself
/// with cookie-generation bumps; a flight that observes a bump between
/// fingerprinting and storing must return its result to the waiting
/// caller but leave nothing in either cache layer. The fingerprint is
/// deliberately left unchanged here so the test proves the generation
/// alone suppresses caching.
#[tokio::test]
async fn extraction_overlapping_a_cookie_change_is_returned_but_not_cached() {
    let app = boot().await;
    let ytdlp_dir = tempfile::tempdir().unwrap();
    let ytdlp_path = ytdlp_dir.path().join("yt-dlp");
    let counter_path = ytdlp_dir.path().join("invocations");
    let started_path = ytdlp_dir.path().join("started");
    let now = chrono::Utc::now().timestamp();
    let output = metadata(
        "straddles-cookie-change",
        &format!("https://media.example/v?expire={}", now + 7200),
    )
    .to_string();
    write_script(
        &ytdlp_path,
        &format!(
            "#!/bin/sh\nprintf 'run\\n' >> '{counter}'\ntouch '{started}'\nsleep 1\nprintf '%s\\n' '{output}'\n",
            counter = counter_path.display(),
            started = started_path.display(),
        ),
    );

    let mut cfg = Config::from_env().unwrap();
    cfg.ytdlp_path = ytdlp_path.to_string_lossy().into_owned();
    let cache = Arc::new(VideoCache::new());
    let flight_cache = Arc::clone(&cache);
    let flight_pool = app.pool.clone();
    let flight_cfg = cfg.clone();
    let flight = tokio::spawn(async move {
        flight_cache
            .get_or_extract(&flight_pool, &flight_cfg, "straddles-cookie-change")
            .await
    });

    for _ in 0..100 {
        if started_path.exists() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(started_path.exists());
    // The cookie route runs to completion while yt-dlp is still going.
    ytdlp::begin_cookie_change();
    ytdlp::forget_sabr_only_session();

    let result = flight.await.unwrap().unwrap();
    assert_eq!(result.formats.len(), 1, "the caller still gets the result");

    let rows: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM video_metadata_cache WHERE video_id = ?")
            .bind("straddles-cookie-change")
            .fetch_one(&app.pool)
            .await
            .unwrap();
    assert_eq!(
        rows, 0,
        "a result that straddled the change must not be persisted"
    );

    // Same fingerprint, same cache instance: the next caller must still
    // re-extract, proving the in-memory layer was not populated either.
    cache
        .get_or_extract(&app.pool, &cfg, "straddles-cookie-change")
        .await
        .unwrap();
    let invocations = std::fs::read_to_string(counter_path).unwrap();
    assert_eq!(invocations.lines().count(), 2);
}
