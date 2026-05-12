//! Direct unit tests of pure service helpers that don't need the full
//! HTTP harness.

mod common;

use common::boot;
use hometube::models::account::AccountType;
use hometube::services::cron::{compute_next_run_at, seed_default_jobs, seed_ytdlp_info};
use hometube::services::notifications::{self, dispatch_ytdlp_failure_deduped, TYPE_SYSTEM_UPDATE};
use hometube::services::setup::{has_first_parent, has_google_credentials, is_setup_complete};
use hometube::services::video_cache::{
    cache_size_preset_to_bytes, current_cache_size_label, evict_video_public, list_cached_videos,
    set_cache_size, set_ttl_hours, total_cache_bytes, total_segment_count, CACHE_SIZE_PRESETS,
};

#[tokio::test]
async fn setup_helpers_reflect_db_state() {
    let app = boot().await;

    assert!(!is_setup_complete(&app.pool).await.unwrap());
    assert!(!has_google_credentials(&app.pool).await.unwrap());
    assert!(!has_first_parent(&app.pool).await.unwrap());

    common::seed_credentials(&app.pool).await;
    assert!(has_google_credentials(&app.pool).await.unwrap());

    common::insert_account(&app.pool, "g1", "p@e.t", "P", AccountType::Parent).await;
    assert!(has_first_parent(&app.pool).await.unwrap());
}

#[tokio::test]
async fn seed_default_jobs_is_idempotent() {
    let app = boot().await;
    seed_default_jobs(&app.pool).await.unwrap();
    let count1: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM cron_jobs")
        .fetch_one(&app.pool)
        .await
        .unwrap();
    assert_eq!(count1, 2);

    // Re-seed → still two rows (INSERT OR IGNORE).
    seed_default_jobs(&app.pool).await.unwrap();
    let count2: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM cron_jobs")
        .fetch_one(&app.pool)
        .await
        .unwrap();
    assert_eq!(count2, 2);
}

#[tokio::test]
async fn seed_ytdlp_info_inserts_singleton() {
    let app = boot().await;
    let cfg = hometube::config::Config::from_env().unwrap();
    seed_ytdlp_info(&app.pool, &cfg).await.unwrap();
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM ytdlp_info")
        .fetch_one(&app.pool)
        .await
        .unwrap();
    assert_eq!(count, 1);

    // Idempotent.
    seed_ytdlp_info(&app.pool, &cfg).await.unwrap();
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM ytdlp_info")
        .fetch_one(&app.pool)
        .await
        .unwrap();
    assert_eq!(count, 1);
}

#[tokio::test]
async fn cron_compute_next_run_at_handles_each_preset() {
    for expr in &["*/15 * * * *", "0 * * * *", "0 3 * * *"] {
        assert!(
            compute_next_run_at(expr).is_some(),
            "{expr} should parse as cron"
        );
    }
    // Garbage doesn't.
    assert!(compute_next_run_at("definitely not cron").is_none());
}

#[tokio::test]
async fn dispatch_ytdlp_failure_dedups() {
    let app = boot().await;
    common::insert_account(&app.pool, "g1", "p@e.t", "P", AccountType::Parent).await;

    // First call → notification inserted.
    dispatch_ytdlp_failure_deduped(&app.pool, "vid-X", "boom")
        .await
        .unwrap();
    let count1: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM parent_notifications WHERE notification_type = 'ytdlp_failure'",
    )
    .fetch_one(&app.pool)
    .await
    .unwrap();
    assert_eq!(count1, 1);

    // Second call for the same video within the dedup window → no new
    // notification.
    dispatch_ytdlp_failure_deduped(&app.pool, "vid-X", "boom again")
        .await
        .unwrap();
    let count2: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM parent_notifications WHERE notification_type = 'ytdlp_failure'",
    )
    .fetch_one(&app.pool)
    .await
    .unwrap();
    assert_eq!(count2, 1);
}

#[tokio::test]
async fn broadcast_once_per_day_dedups() {
    let app = boot().await;
    common::insert_account(&app.pool, "g1", "p@e.t", "P", AccountType::Parent).await;

    let metadata = serde_json::json!({"child_account_id": 1});
    let key = notifications::json_fragment_key("child_account_id", &1);

    notifications::broadcast_once_per_day(
        &app.pool,
        TYPE_SYSTEM_UPDATE,
        &key,
        "title",
        "msg",
        &metadata,
    )
    .await
    .unwrap();
    notifications::broadcast_once_per_day(
        &app.pool,
        TYPE_SYSTEM_UPDATE,
        &key,
        "title",
        "msg",
        &metadata,
    )
    .await
    .unwrap();

    let count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM parent_notifications WHERE notification_type = ?")
            .bind(TYPE_SYSTEM_UPDATE)
            .fetch_one(&app.pool)
            .await
            .unwrap();
    assert_eq!(count, 1);
}

#[tokio::test]
async fn video_cache_helpers() {
    let app = boot().await;

    // Default + change.
    assert_eq!(current_cache_size_label(&app.pool).await, "50 GB");
    set_cache_size(&app.pool, "10 GB").await.unwrap();
    assert_eq!(current_cache_size_label(&app.pool).await, "10 GB");

    // Invalid label → error.
    assert!(set_cache_size(&app.pool, "weird").await.is_err());

    // Bytes math.
    assert_eq!(cache_size_preset_to_bytes("5 GB"), 5 * 1024 * 1024 * 1024);

    // TTL.
    set_ttl_hours(&app.pool, 12).await.unwrap();

    // Cache stats on an empty DB.
    assert_eq!(total_cache_bytes(&app.pool).await.unwrap(), 0);
    assert_eq!(total_segment_count(&app.pool).await.unwrap(), 0);
    assert!(list_cached_videos(&app.pool).await.unwrap().is_empty());

    // Evict a non-existent video → returns (0, 0).
    let (segs, bytes) = evict_video_public(&app.pool, "no-such-vid").await.unwrap();
    assert_eq!(segs, 0);
    assert_eq!(bytes, 0);
}

#[tokio::test]
async fn cache_size_presets_constant_is_complete() {
    assert!(CACHE_SIZE_PRESETS.contains(&"5 GB"));
    assert!(CACHE_SIZE_PRESETS.contains(&"Unlimited"));
}
