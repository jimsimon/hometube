//! Regression coverage for yt-dlp metadata cache freshness.

mod common;

use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use common::boot;
use hometube::config::Config;
use hometube::services::setup::{set_config_value, KEY_YTDLP_COOKIES};
use hometube::services::video_cache::{current_extractor_config_key, VideoCache};
use hometube::services::ytdlp;

static COOKIE_TEST_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

fn metadata(video_id: &str, media_url: &str) -> serde_json::Value {
    serde_json::json!({
        "id": video_id,
        "title": "Cache freshness test",
        "formats": [{
            "format_id": "251",
            "ext": "webm",
            "url": media_url,
            "protocol": "https",
            "filesize": 4096,
            "acodec": "opus",
            "vcodec": "none"
        }],
        "format_box_ranges": {
            "251": {
                "init_start": 0,
                "init_end": 255,
                "index_start": 256,
                "index_end": 511
            }
        },
        "thumbnails": []
    })
}

fn write_fake_ytdlp(path: &Path, output: &serde_json::Value) {
    let script = format!("#!/bin/sh\nprintf '%s\\n' '{}'\n", output);
    write_script(path, &script);
}

fn write_script(path: &Path, script: &str) {
    std::fs::write(path, script).expect("write fake yt-dlp");
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755))
        .expect("chmod fake yt-dlp");
}

#[tokio::test]
async fn stale_extractor_config_and_expired_urls_are_reextracted() {
    let _cookie_guard = COOKIE_TEST_LOCK.lock().await;
    let app = boot().await;
    let _ = std::fs::remove_file(ytdlp::cookies_file_path());
    let video_id = "cache-fresh";
    let now = chrono::Utc::now().timestamp();
    let stale = metadata(
        video_id,
        &format!("https://media.example/stale?expire={}", now + 3600),
    );

    sqlx::query(
        "INSERT INTO video_metadata_cache \
            (video_id, metadata_json, cached_at, expires_at, extractor_config_key) \
         VALUES (?, ?, ?, ?, 'old-player-client')",
    )
    .bind(video_id)
    .bind(stale.to_string())
    .bind(now)
    .bind(now + 3600)
    .execute(&app.pool)
    .await
    .unwrap();

    let ytdlp_dir = tempfile::tempdir().unwrap();
    let ytdlp_path = ytdlp_dir.path().join("yt-dlp");
    let fresh_url = format!("https://media.example/fresh?expire={}", now + 7200);
    write_fake_ytdlp(&ytdlp_path, &metadata(video_id, &fresh_url));

    let mut cfg = Config::from_env().unwrap();
    cfg.ytdlp_path = ytdlp_path.to_string_lossy().into_owned();

    let extracted = VideoCache::new()
        .get_or_extract(&app.pool, &cfg, video_id)
        .await
        .unwrap();
    assert_eq!(
        extracted.formats[0].url.as_deref(),
        Some(fresh_url.as_str())
    );

    let stored_key: String = sqlx::query_scalar(
        "SELECT extractor_config_key FROM video_metadata_cache WHERE video_id = ?",
    )
    .bind(video_id)
    .fetch_one(&app.pool)
    .await
    .unwrap();
    assert_ne!(stored_key, "old-player-client");

    // A fresh row is a real DB cache hit. If this unexpectedly shells out,
    // the deliberately broken executable makes the assertion fail.
    std::fs::write(&ytdlp_path, "#!/bin/sh\nexit 99\n").unwrap();
    let cached = VideoCache::new()
        .get_or_extract(&app.pool, &cfg, video_id)
        .await
        .unwrap();
    assert_eq!(cached.formats[0].url.as_deref(), Some(fresh_url.as_str()));

    // Keep the DB TTL and extractor key valid but replace the media URL with
    // an expired one. URL freshness must independently force extraction.
    let expired = metadata(
        video_id,
        &format!("https://media.example/expired?expire={}", now - 60),
    );
    sqlx::query(
        "UPDATE video_metadata_cache SET metadata_json = ?, expires_at = ? WHERE video_id = ?",
    )
    .bind(expired.to_string())
    .bind(now + 3600)
    .bind(video_id)
    .execute(&app.pool)
    .await
    .unwrap();
    write_fake_ytdlp(&ytdlp_path, &metadata(video_id, &fresh_url));

    let refreshed = VideoCache::new()
        .get_or_extract(&app.pool, &cfg, video_id)
        .await
        .unwrap();
    assert_eq!(
        refreshed.formats[0].url.as_deref(),
        Some(fresh_url.as_str())
    );
}

#[tokio::test]
async fn newly_extracted_urls_inside_the_expiry_margin_are_rejected() {
    let _cookie_guard = COOKIE_TEST_LOCK.lock().await;
    let app = boot().await;
    let _ = std::fs::remove_file(ytdlp::cookies_file_path());
    let video_id = "nearly-expired-extraction";
    let now = chrono::Utc::now().timestamp();
    let ytdlp_dir = tempfile::tempdir().unwrap();
    let ytdlp_path = ytdlp_dir.path().join("yt-dlp");
    let stale_url = format!("https://media.example/stale?expire={}", now + 60);
    write_fake_ytdlp(&ytdlp_path, &metadata(video_id, &stale_url));

    let mut cfg = Config::from_env().unwrap();
    cfg.ytdlp_path = ytdlp_path.to_string_lossy().into_owned();
    let error = VideoCache::new()
        .get_or_extract(&app.pool, &cfg, video_id)
        .await
        .unwrap_err()
        .to_string();
    assert!(
        error.contains("expire within 300 seconds"),
        "error was: {error}"
    );

    let stored: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM video_metadata_cache WHERE video_id = ?")
            .bind(video_id)
            .fetch_one(&app.pool)
            .await
            .unwrap();
    assert_eq!(stored, 0);
}

#[tokio::test]
async fn extractor_config_key_tracks_binary_path_and_uploaded_cookies() {
    let _cookie_guard = COOKIE_TEST_LOCK.lock().await;
    let app = boot().await;
    let cookies_path = ytdlp::cookies_file_path();
    let _ = std::fs::remove_file(&cookies_path);

    let mut cfg = Config::from_env().unwrap();
    cfg.ytdlp_path = "/opt/yt-dlp-a".to_string();
    let first = current_extractor_config_key(&app.pool, &cfg).await.unwrap();

    cfg.ytdlp_path = "/opt/yt-dlp-b".to_string();
    let changed_binary = current_extractor_config_key(&app.pool, &cfg).await.unwrap();
    assert_ne!(first, changed_binary);

    // A new jar uploaded through the parent UI changes the fingerprint.
    set_config_value(&app.pool, KEY_YTDLP_COOKIES, "cookie-upload-one")
        .await
        .unwrap();
    let first_upload = current_extractor_config_key(&app.pool, &cfg).await.unwrap();
    assert_ne!(changed_binary, first_upload);
    set_config_value(&app.pool, KEY_YTDLP_COOKIES, "cookie-upload-two")
        .await
        .unwrap();
    let second_upload = current_extractor_config_key(&app.pool, &cfg).await.unwrap();
    assert_ne!(first_upload, second_upload);

    // yt-dlp rewriting the on-disk jar (rotated session cookies) must
    // NOT change the fingerprint — otherwise every yt-dlp run for any
    // video would flush the metadata cache for every other video.
    std::fs::write(&cookies_path, "cookie-version-one").unwrap();
    let first_cookie = current_extractor_config_key(&app.pool, &cfg).await.unwrap();
    std::fs::write(&cookies_path, "cookie-version-two").unwrap();
    let second_cookie = current_extractor_config_key(&app.pool, &cfg).await.unwrap();
    assert_eq!(second_upload, first_cookie);
    assert_eq!(first_cookie, second_cookie);

    let _ = std::fs::remove_file(cookies_path);
}

#[tokio::test]
async fn extraction_is_stored_under_the_refreshed_cookie_fingerprint() {
    let _cookie_guard = COOKIE_TEST_LOCK.lock().await;
    let app = boot().await;
    let cookies_path = ytdlp::cookies_file_path();
    std::fs::write(
        &cookies_path,
        "# Netscape HTTP Cookie File\n.youtube.com\tTRUE\t/\tTRUE\t2147483647\tSID\toriginal\n",
    )
    .unwrap();

    let now = chrono::Utc::now().timestamp();
    let video_id = "rotated-cookie-key";
    let media_url = format!("https://media.example/rotated?expire={}", now + 7200);
    let output = metadata(video_id, &media_url);
    let ytdlp_dir = tempfile::tempdir().unwrap();
    let ytdlp_path = ytdlp_dir.path().join("yt-dlp");
    write_script(
        &ytdlp_path,
        &format!(
            "#!/bin/sh\ncookies=''\nwhile [ \"$#\" -gt 0 ]; do\n  if [ \"$1\" = '--cookies' ]; then cookies=$2; shift 2; else shift; fi\ndone\nif [ -n \"$cookies\" ]; then\n  sed 's/original/refreshed/' \"$cookies\" > \"$cookies.next\"\n  mv \"$cookies.next\" \"$cookies\"\nfi\nprintf '%s\\n' '{}'\n",
            output
        ),
    );

    let mut cfg = Config::from_env().unwrap();
    cfg.ytdlp_path = ytdlp_path.to_string_lossy().into_owned();
    let cache = VideoCache::new();
    let extracted = cache
        .get_or_extract(&app.pool, &cfg, video_id)
        .await
        .unwrap();
    assert_eq!(
        extracted.formats[0].url.as_deref(),
        Some(media_url.as_str())
    );
    assert!(std::fs::read_to_string(&cookies_path)
        .unwrap()
        .contains("refreshed"));

    // The next request must hit the cache keyed by the rewritten canonical
    // jar. A second extraction would execute this deliberately broken shim.
    write_script(&ytdlp_path, "#!/bin/sh\nexit 99\n");
    let cached = VideoCache::new()
        .get_or_extract(&app.pool, &cfg, video_id)
        .await
        .unwrap();
    assert_eq!(cached.formats[0].url.as_deref(), Some(media_url.as_str()));

    let _ = std::fs::remove_file(cookies_path);
}

#[tokio::test]
async fn database_hit_keeps_its_persisted_expiration_in_memory() {
    let _cookie_guard = COOKIE_TEST_LOCK.lock().await;
    let app = boot().await;
    let _ = std::fs::remove_file(ytdlp::cookies_file_path());
    let now = chrono::Utc::now().timestamp();
    let video_id = "persisted-expiry";

    let ytdlp_dir = tempfile::tempdir().unwrap();
    let ytdlp_path = ytdlp_dir.path().join("yt-dlp");
    let refreshed_url = format!("https://media.example/refreshed?expire={}", now + 7200);
    write_fake_ytdlp(&ytdlp_path, &metadata(video_id, &refreshed_url));
    let mut cfg = Config::from_env().unwrap();
    cfg.ytdlp_path = ytdlp_path.to_string_lossy().into_owned();
    let config_key = current_extractor_config_key(&app.pool, &cfg).await.unwrap();

    let cached_url = format!("https://media.example/cached?expire={}", now + 7200);
    sqlx::query(
        "INSERT INTO video_metadata_cache \
            (video_id, metadata_json, cached_at, expires_at, extractor_config_key) \
         VALUES (?, ?, ?, ?, ?)",
    )
    .bind(video_id)
    .bind(metadata(video_id, &cached_url).to_string())
    .bind(now)
    .bind(now + 2)
    .bind(config_key)
    .execute(&app.pool)
    .await
    .unwrap();

    let cache = VideoCache::new();
    let cached = cache
        .get_or_extract(&app.pool, &cfg, video_id)
        .await
        .unwrap();
    assert_eq!(cached.formats[0].url.as_deref(), Some(cached_url.as_str()));

    tokio::time::sleep(Duration::from_millis(2200)).await;
    let refreshed = cache
        .get_or_extract(&app.pool, &cfg, video_id)
        .await
        .unwrap();
    assert_eq!(
        refreshed.formats[0].url.as_deref(),
        Some(refreshed_url.as_str())
    );
}

#[tokio::test]
async fn concurrent_callers_share_the_same_extraction_failure() {
    let _cookie_guard = COOKIE_TEST_LOCK.lock().await;
    let app = boot().await;
    let _ = std::fs::remove_file(ytdlp::cookies_file_path());
    let ytdlp_dir = tempfile::tempdir().unwrap();
    let ytdlp_path = ytdlp_dir.path().join("yt-dlp");
    let counter_path = ytdlp_dir.path().join("invocations");
    write_script(
        &ytdlp_path,
        &format!(
            "#!/bin/sh\nprintf 'run\\n' >> '{}'\nsleep 1\nprintf 'simulated extraction failure\\n' >&2\nexit 42\n",
            counter_path.display()
        ),
    );

    let mut cfg = Config::from_env().unwrap();
    cfg.ytdlp_path = ytdlp_path.to_string_lossy().into_owned();
    let cache = Arc::new(VideoCache::new());
    let first_cache = Arc::clone(&cache);
    let second_cache = Arc::clone(&cache);
    let first_pool = app.pool.clone();
    let second_pool = app.pool.clone();
    let first_cfg = cfg.clone();
    let second_cfg = cfg.clone();

    let first = tokio::spawn(async move {
        first_cache
            .get_or_extract(&first_pool, &first_cfg, "shared-failure")
            .await
    });
    let second = tokio::spawn(async move {
        second_cache
            .get_or_extract(&second_pool, &second_cfg, "shared-failure")
            .await
    });
    let first_error = first.await.unwrap().unwrap_err().to_string();
    let second_error = second.await.unwrap().unwrap_err().to_string();

    assert_eq!(first_error, second_error);
    let invocations = std::fs::read_to_string(counter_path).unwrap();
    assert_eq!(invocations.lines().count(), 1);
}

#[tokio::test]
async fn concurrent_different_configurations_use_separate_extraction_flights() {
    let _cookie_guard = COOKIE_TEST_LOCK.lock().await;
    let app = boot().await;
    let _ = std::fs::remove_file(ytdlp::cookies_file_path());
    let ytdlp_dir = tempfile::tempdir().unwrap();
    let first_path = ytdlp_dir.path().join("yt-dlp-first");
    let second_path = ytdlp_dir.path().join("yt-dlp-second");
    let now = chrono::Utc::now().timestamp();
    let first_url = format!("https://media.example/first?expire={}", now + 7200);
    let second_url = format!("https://media.example/second?expire={}", now + 7200);
    for (path, output) in [
        (&first_path, metadata("separate-flights", &first_url)),
        (&second_path, metadata("separate-flights", &second_url)),
    ] {
        write_script(
            path,
            &format!("#!/bin/sh\nsleep 1\nprintf '%s\\n' '{}'\n", output),
        );
    }

    let mut first_cfg = Config::from_env().unwrap();
    first_cfg.ytdlp_path = first_path.to_string_lossy().into_owned();
    let mut second_cfg = first_cfg.clone();
    second_cfg.ytdlp_path = second_path.to_string_lossy().into_owned();
    let cache = Arc::new(VideoCache::new());
    let first_cache = Arc::clone(&cache);
    let second_cache = Arc::clone(&cache);
    let first_pool = app.pool.clone();
    let second_pool = app.pool.clone();

    let first = tokio::spawn(async move {
        first_cache
            .get_or_extract(&first_pool, &first_cfg, "separate-flights")
            .await
            .unwrap()
    });
    let second = tokio::spawn(async move {
        second_cache
            .get_or_extract(&second_pool, &second_cfg, "separate-flights")
            .await
            .unwrap()
    });
    let (first_result, second_result) = tokio::join!(first, second);

    assert_eq!(
        first_result.unwrap().formats[0].url.as_deref(),
        Some(first_url.as_str())
    );
    assert_eq!(
        second_result.unwrap().formats[0].url.as_deref(),
        Some(second_url.as_str())
    );
}

#[tokio::test]
async fn fingerprint_change_during_extraction_does_not_start_a_second_run() {
    let _cookie_guard = COOKIE_TEST_LOCK.lock().await;
    let app = boot().await;
    let cookies_path = ytdlp::cookies_file_path();
    let _ = std::fs::remove_file(&cookies_path);
    let ytdlp_dir = tempfile::tempdir().unwrap();
    let ytdlp_path = ytdlp_dir.path().join("yt-dlp");
    let counter_path = ytdlp_dir.path().join("invocations");
    let started_path = ytdlp_dir.path().join("started");
    let now = chrono::Utc::now().timestamp();
    let media_url = format!("https://media.example/rotated?expire={}", now + 7200);
    let output = metadata("serialized-cookie-rotation", &media_url);
    write_script(
        &ytdlp_path,
        &format!(
            "#!/bin/sh\nprintf 'run\\n' >> '{}'\ntouch '{}'\nsleep 1\nprintf '%s\\n' '{}'\n",
            counter_path.display(),
            started_path.display(),
            output
        ),
    );

    let mut cfg = Config::from_env().unwrap();
    cfg.ytdlp_path = ytdlp_path.to_string_lossy().into_owned();
    let first_key = current_extractor_config_key(&app.pool, &cfg).await.unwrap();
    let cache = Arc::new(VideoCache::new());
    let first_cache = Arc::clone(&cache);
    let first_pool = app.pool.clone();
    let first_cfg = cfg.clone();
    let first = tokio::spawn(async move {
        first_cache
            .get_or_extract(&first_pool, &first_cfg, "serialized-cookie-rotation")
            .await
    });

    for _ in 0..100 {
        if started_path.exists() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(started_path.exists());
    // A parent uploads a new cookie jar while the first flight is running.
    set_config_value(&app.pool, KEY_YTDLP_COOKIES, "uploaded-mid-flight")
        .await
        .unwrap();
    let changed_key = current_extractor_config_key(&app.pool, &cfg).await.unwrap();
    assert_ne!(first_key, changed_key);

    let second_cache = Arc::clone(&cache);
    let second_pool = app.pool.clone();
    let second = tokio::spawn(async move {
        second_cache
            .get_or_extract(&second_pool, &cfg, "serialized-cookie-rotation")
            .await
    });
    let (first_result, second_result) = tokio::join!(first, second);
    for result in [first_result.unwrap(), second_result.unwrap()] {
        assert_eq!(
            result.unwrap().formats[0].url.as_deref(),
            Some(media_url.as_str())
        );
    }
    let invocations = std::fs::read_to_string(counter_path).unwrap();
    assert_eq!(invocations.lines().count(), 1);

    let _ = std::fs::remove_file(cookies_path);
}
