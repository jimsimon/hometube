//! Tests for the video_cache cleanup (allowlist-based eviction and LRU).

mod common;

use chrono::Utc;
use common::boot;
use hometube::services::video_cache::{cleanup_segment_cache, set_cache_size};

/// Seed a segment_cache row and its associated video_metadata_cache.
async fn seed_segment(pool: &sqlx::SqlitePool, video_id: &str, size: i64) {
    let json = serde_json::json!({
        "id": video_id,
        "channel_id": "UCorphan",
        "formats": [],
        "thumbnails": [],
        "subtitles": {},
        "automatic_captions": {}
    });
    let expires_at = Utc::now().timestamp() + 3600;
    sqlx::query(
        "INSERT OR IGNORE INTO video_metadata_cache (video_id, metadata_json, expires_at) \
         VALUES (?, ?, ?)",
    )
    .bind(video_id)
    .bind(json.to_string())
    .bind(expires_at)
    .execute(pool)
    .await
    .unwrap();

    sqlx::query(
        "INSERT INTO segment_cache (video_id, format_id, segment_number, file_path, file_size_bytes, last_accessed_at) \
         VALUES (?, '137', 0, '/tmp/nonexistent_segment', ?, ?)",
    )
    .bind(video_id)
    .bind(size)
    .bind(Utc::now().timestamp())
    .execute(pool)
    .await
    .unwrap();
}

// ---------------------------------------------------------------------------
// Allowlist-based cleanup
// ---------------------------------------------------------------------------

#[tokio::test]
async fn cleanup_evicts_segments_not_in_any_allowlist() {
    let app = boot().await;
    // Seed an orphaned video that no child has allowlisted.
    seed_segment(&app.pool, "orphan-vid", 4096).await;

    let (msg, _output) = cleanup_segment_cache(&app.pool).await.unwrap();
    assert!(msg.contains("1 videos"), "msg was: {msg}");

    // segment_cache should be empty.
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM segment_cache")
        .fetch_one(&app.pool)
        .await
        .unwrap();
    assert_eq!(count, 0);
}

#[tokio::test]
async fn cleanup_keeps_directly_allowlisted_video() {
    let app = boot().await;
    seed_segment(&app.pool, "keep-vid", 4096).await;

    // Create a child and allowlist the video.
    let child_id = common::insert_account(
        &app.pool,
        "g-child",
        "c@t.com",
        "Kid",
        hometube::models::account::AccountType::Child,
    )
    .await;
    sqlx::query(
        "INSERT INTO allowlisted_videos (child_account_id, video_id, video_title, added_by) \
         VALUES (?, 'keep-vid', 'Keep', ?)",
    )
    .bind(child_id)
    .bind(child_id)
    .execute(&app.pool)
    .await
    .unwrap();

    let (msg, _) = cleanup_segment_cache(&app.pool).await.unwrap();
    assert!(msg.contains("0 videos"), "msg was: {msg}");

    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM segment_cache")
        .fetch_one(&app.pool)
        .await
        .unwrap();
    assert_eq!(count, 1);
}

#[tokio::test]
async fn cleanup_keeps_channel_allowlisted_video() {
    let app = boot().await;

    // Seed with channel_id = "UCkeep".
    let json = serde_json::json!({
        "id": "ch-vid",
        "channel_id": "UCkeep",
        "formats": [],
        "thumbnails": [],
        "subtitles": {},
        "automatic_captions": {}
    });
    let expires_at = Utc::now().timestamp() + 3600;
    sqlx::query(
        "INSERT INTO video_metadata_cache (video_id, metadata_json, expires_at) VALUES (?, ?, ?)",
    )
    .bind("ch-vid")
    .bind(json.to_string())
    .bind(expires_at)
    .execute(&app.pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO segment_cache (video_id, format_id, segment_number, file_path, file_size_bytes) \
         VALUES ('ch-vid', '137', 0, '/tmp/fake.seg', 1024)",
    )
    .execute(&app.pool)
    .await
    .unwrap();

    // Allowlist the channel.
    let child_id = common::insert_account(
        &app.pool,
        "g-child",
        "c@t.com",
        "Kid",
        hometube::models::account::AccountType::Child,
    )
    .await;
    sqlx::query(
        "INSERT INTO allowlisted_channels (child_account_id, channel_id, channel_title, added_by) \
         VALUES (?, 'UCkeep', 'Keep Channel', ?)",
    )
    .bind(child_id)
    .bind(child_id)
    .execute(&app.pool)
    .await
    .unwrap();

    let (msg, _) = cleanup_segment_cache(&app.pool).await.unwrap();
    assert!(msg.contains("0 videos"), "msg was: {msg}");
}

// ---------------------------------------------------------------------------
// LRU eviction
// ---------------------------------------------------------------------------

#[tokio::test]
async fn lru_eviction_when_over_limit() {
    let app = boot().await;

    // Set cache max to 5 GB.
    set_cache_size(&app.pool, "5 GB").await.unwrap();

    // Seed two segments with enormous sizes to exceed the limit.
    // (We fake the sizes — no real files needed for the DB logic.)
    let child_id = common::insert_account(
        &app.pool,
        "g-child",
        "c@t.com",
        "Kid",
        hometube::models::account::AccountType::Child,
    )
    .await;
    // Allowlist both videos so the allowlist cleanup doesn't remove them.
    for vid in ["big-a", "big-b"] {
        sqlx::query(
            "INSERT INTO allowlisted_videos (child_account_id, video_id, video_title, added_by) \
             VALUES (?, ?, 'V', ?)",
        )
        .bind(child_id)
        .bind(vid)
        .bind(child_id)
        .execute(&app.pool)
        .await
        .unwrap();
    }

    // big-a: 3 GB, accessed a while ago (LRU).
    sqlx::query(
        "INSERT INTO segment_cache (video_id, format_id, segment_number, file_path, file_size_bytes, last_accessed_at) \
         VALUES ('big-a', '137', 0, '/tmp/nonexistent_a', ?, 1000)",
    )
    .bind(3_i64 * 1024 * 1024 * 1024)
    .execute(&app.pool)
    .await
    .unwrap();

    // big-b: 3 GB, accessed recently.
    sqlx::query(
        "INSERT INTO segment_cache (video_id, format_id, segment_number, file_path, file_size_bytes, last_accessed_at) \
         VALUES ('big-b', '137', 0, '/tmp/nonexistent_b', ?, ?)",
    )
    .bind(3_i64 * 1024 * 1024 * 1024)
    .bind(Utc::now().timestamp())
    .execute(&app.pool)
    .await
    .unwrap();

    // Total: 6 GB > 5 GB limit.
    let (msg, _) = cleanup_segment_cache(&app.pool).await.unwrap();
    assert!(msg.contains("freed"), "msg was: {msg}");

    // The LRU segment (big-a) should be evicted. big-b stays.
    let remaining: Vec<(String,)> = sqlx::query_as("SELECT video_id FROM segment_cache")
        .fetch_all(&app.pool)
        .await
        .unwrap();
    assert_eq!(remaining.len(), 1);
    assert_eq!(remaining[0].0, "big-b");
}

#[tokio::test]
async fn cleanup_noop_when_empty() {
    let app = boot().await;
    let (msg, _) = cleanup_segment_cache(&app.pool).await.unwrap();
    assert!(msg.contains("0 videos"), "msg was: {msg}");
    assert!(msg.contains("0 segments"), "msg was: {msg}");
}
