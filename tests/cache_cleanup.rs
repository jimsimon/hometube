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

    // Set cache max to 10 GB.
    set_cache_size(&app.pool, "10 GB").await.unwrap();

    // Seed two segments with enormous sizes to exceed the limit.
    // (We fake the sizes — no real files needed for the DB logic.)
    let child_id = common::insert_account(
        &app.pool,
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

    // big-a: 6 GB, accessed a while ago (LRU).
    sqlx::query(
        "INSERT INTO segment_cache (video_id, format_id, segment_number, file_path, file_size_bytes, last_accessed_at) \
         VALUES ('big-a', '137', 0, '/tmp/nonexistent_a', ?, 1000)",
    )
    .bind(6_i64 * 1024 * 1024 * 1024)
    .execute(&app.pool)
    .await
    .unwrap();

    // big-b: 6 GB, accessed recently.
    sqlx::query(
        "INSERT INTO segment_cache (video_id, format_id, segment_number, file_path, file_size_bytes, last_accessed_at) \
         VALUES ('big-b', '137', 0, '/tmp/nonexistent_b', ?, ?)",
    )
    .bind(6_i64 * 1024 * 1024 * 1024)
    .bind(Utc::now().timestamp())
    .execute(&app.pool)
    .await
    .unwrap();

    // Total: 12 GB > 10 GB limit.
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

#[tokio::test]
async fn allowlist_eviction_logs_reason_with_timestamp() {
    let app = boot().await;
    seed_segment(&app.pool, "orphan-vid", 2048).await;

    let before = Utc::now().timestamp();
    cleanup_segment_cache(&app.pool).await.unwrap();
    let after = Utc::now().timestamp();

    let rows: Vec<(String, String, i64, i64, i64)> = sqlx::query_as(
        "SELECT video_id, reason, segment_count, bytes_freed, evicted_at \
         FROM cache_evictions",
    )
    .fetch_all(&app.pool)
    .await
    .unwrap();
    assert_eq!(rows.len(), 1, "expected one eviction row, got {rows:?}");
    let (video_id, reason, segs, bytes, evicted_at) = &rows[0];
    assert_eq!(video_id, "orphan-vid");
    assert_eq!(reason, "not_allowlisted");
    assert_eq!(*segs, 1);
    assert_eq!(*bytes, 2048);
    assert!(
        *evicted_at >= before && *evicted_at <= after,
        "evicted_at {evicted_at} not in [{before}, {after}]"
    );
}

#[tokio::test]
async fn unlimited_size_skips_lru_eviction_even_when_huge() {
    use hometube::services::video_cache::set_cache_size;
    let app = boot().await;
    set_cache_size(&app.pool, "Unlimited").await.unwrap();

    // Allowlist + seed a huge segment so the allowlist pass keeps it
    // and only the LRU path could possibly evict it.
    let child_id = common::insert_account(
        &app.pool,
        "Kid",
        hometube::models::account::AccountType::Child,
    )
    .await;
    sqlx::query(
        "INSERT INTO allowlisted_videos (child_account_id, video_id, video_title, added_by) \
         VALUES (?, 'huge-vid', 'V', ?)",
    )
    .bind(child_id)
    .bind(child_id)
    .execute(&app.pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO segment_cache (video_id, format_id, segment_number, file_path, file_size_bytes, last_accessed_at) \
         VALUES ('huge-vid', '137', 0, '/tmp/nonexistent_huge', ?, 1000)",
    )
    .bind(999_i64 * 1024 * 1024 * 1024) // 999 GB
    .execute(&app.pool)
    .await
    .unwrap();

    cleanup_segment_cache(&app.pool).await.unwrap();

    let remaining: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM segment_cache")
        .fetch_one(&app.pool)
        .await
        .unwrap();
    assert_eq!(remaining, 1, "Unlimited must not LRU-evict");
    let lru_log: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM cache_evictions WHERE reason = 'lru_size_limit'",
    )
    .fetch_one(&app.pool)
    .await
    .unwrap();
    assert_eq!(lru_log, 0, "no LRU eviction rows should be logged");
}

#[tokio::test]
async fn manual_evict_logs_reason() {
    use hometube::services::video_cache::evict_video_public;
    let app = boot().await;
    seed_segment(&app.pool, "manual-vid", 1024).await;
    evict_video_public(&app.pool, "manual-vid").await.unwrap();

    let reason: String =
        sqlx::query_scalar("SELECT reason FROM cache_evictions WHERE video_id = 'manual-vid'")
            .fetch_one(&app.pool)
            .await
            .unwrap();
    assert_eq!(reason, "manual");
}
