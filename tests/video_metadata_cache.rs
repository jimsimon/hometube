//! Regression coverage for yt-dlp metadata cache freshness.

mod common;

use std::os::unix::fs::PermissionsExt;
use std::path::Path;

use common::boot;
use hometube::config::Config;
use hometube::services::video_cache::VideoCache;

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
        "thumbnails": []
    })
}

fn write_fake_ytdlp(path: &Path, output: &serde_json::Value) {
    let script = format!("#!/bin/sh\nprintf '%s\\n' '{}'\n", output);
    std::fs::write(path, script).expect("write fake yt-dlp");
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755))
        .expect("chmod fake yt-dlp");
}

#[tokio::test]
async fn stale_extractor_config_and_expired_urls_are_reextracted() {
    let app = boot().await;
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
