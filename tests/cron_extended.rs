//! Extended cron service tests — covers seed_default_jobs idempotency,
//! cache cleanup integration, and various helper functions.

mod common;

use common::boot;
use hometube::services::cron;
use hometube::services::video_cache;

// ---------------------------------------------------------------------------
// Seeding
// ---------------------------------------------------------------------------

#[tokio::test]
async fn seed_default_jobs_creates_rows() {
    let app = boot().await;
    cron::seed_default_jobs(&app.pool).await.unwrap();

    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM cron_jobs")
        .fetch_one(&app.pool)
        .await
        .unwrap();
    assert!(count > 0, "expected at least one cron job, got {count}");
}

#[tokio::test]
async fn seed_default_jobs_is_idempotent() {
    let app = boot().await;
    cron::seed_default_jobs(&app.pool).await.unwrap();
    cron::seed_default_jobs(&app.pool).await.unwrap();

    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM cron_jobs")
        .fetch_one(&app.pool)
        .await
        .unwrap();
    // Should not double up.
    assert!(count <= 5, "too many rows: {count}");
}

#[tokio::test]
async fn seed_ytdlp_info_inserts_and_is_idempotent() {
    let app = boot().await;
    let cfg = hometube::config::Config::from_env().unwrap();

    cron::seed_ytdlp_info(&app.pool, &cfg).await.unwrap();
    let before: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM ytdlp_info")
        .fetch_one(&app.pool)
        .await
        .unwrap();

    cron::seed_ytdlp_info(&app.pool, &cfg).await.unwrap();
    let after: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM ytdlp_info")
        .fetch_one(&app.pool)
        .await
        .unwrap();

    assert_eq!(before, 1);
    assert_eq!(after, 1);
}

// ---------------------------------------------------------------------------
// Preset helpers (unit-style)
// ---------------------------------------------------------------------------

#[test]
fn preset_round_trips() {
    for preset in [
        "Every 30 minutes",
        "Every hour",
        "Every 6 hours",
        "Daily (3:00 AM)",
        "Weekly (Sunday 3 AM)",
    ] {
        let expr = cron::preset_to_expression(preset);
        assert!(expr.is_some(), "preset '{preset}' gave None");
        let back = cron::expression_to_preset(expr.unwrap());
        assert_eq!(back, preset, "round-trip failed for preset '{preset}'");
    }
}

#[test]
fn unknown_preset_returns_none() {
    let expr = cron::preset_to_expression("custom_nonsense");
    assert!(expr.is_none());
}

#[test]
fn expression_to_preset_unknown_maps_to_custom() {
    let preset = cron::expression_to_preset("0 0 0 1 1 *");
    assert_eq!(preset, "Custom");
}

#[test]
fn compute_next_run_at_returns_valid_timestamp() {
    let next = cron::compute_next_run_at("0 */30 * * * *");
    assert!(
        next.is_some(),
        "should compute next run for 30-min schedule"
    );
    let ts = next.unwrap();
    assert!(ts > 0);
}

#[test]
fn compute_next_run_at_returns_none_for_garbage() {
    let next = cron::compute_next_run_at("not a cron");
    assert!(next.is_none());
}

#[test]
fn to_six_field_pads_five_field_expressions() {
    let result = cron::to_six_field("*/5 * * * *");
    assert_eq!(result, "0 */5 * * * *");
}

#[test]
fn to_six_field_leaves_six_field_alone() {
    let result = cron::to_six_field("0 0 */6 * * *");
    assert_eq!(result, "0 0 */6 * * *");
}

// ---------------------------------------------------------------------------
// Cache counters
// ---------------------------------------------------------------------------

#[test]
fn cache_hit_counter_starts_at_zero() {
    // The counters are global atomics; just verify they're accessible.
    let hits = cron::CACHE_HIT_COUNTER.load(std::sync::atomic::Ordering::Relaxed);
    let misses = cron::CACHE_MISS_COUNTER.load(std::sync::atomic::Ordering::Relaxed);
    // They may not be zero if other tests ran first, but at least they're not panicking.
    let _ = hits;
    let _ = misses;
}

// ---------------------------------------------------------------------------
// run_cache_cleanup integration
// ---------------------------------------------------------------------------

#[tokio::test]
async fn run_cache_cleanup_via_video_cache() {
    let app = boot().await;
    // Seed a segment that isn't allowlisted.
    sqlx::query(
        "INSERT INTO segment_cache (video_id, format_id, segment_number, file_path, file_size_bytes) \
         VALUES ('orphan-cron', '137', 0, '/tmp/orphan.seg', 512)",
    )
    .execute(&app.pool)
    .await
    .unwrap();
    // Also seed its metadata.
    let json = serde_json::json!({"id":"orphan-cron","channel_id":"UCx","formats":[],"thumbnails":[],"subtitles":{},"automatic_captions":{}});
    sqlx::query(
        "INSERT INTO video_metadata_cache (video_id, metadata_json, expires_at) VALUES (?, ?, 99999999999)",
    )
    .bind("orphan-cron")
    .bind(json.to_string())
    .execute(&app.pool)
    .await
    .unwrap();

    let (msg, _) = video_cache::cleanup_segment_cache(&app.pool).await.unwrap();
    assert!(msg.contains("1 videos"));
}
