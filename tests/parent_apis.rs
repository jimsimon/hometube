//! Broad coverage of parent-only routes that don't need YouTube.

mod common;

use axum::http::StatusCode;
use common::boot_with_parent_and_child;
use hometube::models::account::AccountType;
use serde_json::json;

#[tokio::test]
async fn accounts_list_returns_seeded() {
    let (app, _auth) = boot_with_parent_and_child(AccountType::Parent).await;
    let res = app.server.get("/api/accounts").await;
    assert_eq!(res.status_code(), StatusCode::OK);
    let body: serde_json::Value = res.json();
    assert_eq!(body.as_array().unwrap().len(), 2);
}

#[tokio::test]
async fn accounts_list_filtered_by_type() {
    let (app, _auth) = boot_with_parent_and_child(AccountType::Parent).await;
    let res = app.server.get("/api/accounts?type=child").await;
    let body: serde_json::Value = res.json();
    assert_eq!(body.as_array().unwrap().len(), 1);
    assert_eq!(body[0]["account_type"], "child");

    // Bad filter → 400.
    let res = app.server.get("/api/accounts?type=nonsense").await;
    assert_eq!(res.status_code(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn account_get_and_update_round_trip() {
    let (app, _auth) = boot_with_parent_and_child(AccountType::Parent).await;
    let child_id = app.child_id.unwrap();

    let res = app.server.get(&format!("/api/accounts/{child_id}")).await;
    assert_eq!(res.status_code(), StatusCode::OK);

    let res = app
        .server
        .put(&format!("/api/accounts/{child_id}"))
        .json(&json!({ "display_name": "Renamed Child" }))
        .await;
    assert_eq!(res.status_code(), StatusCode::OK);
    let body: serde_json::Value = res.json();
    assert_eq!(body["display_name"], "Renamed Child");
}

#[tokio::test]
async fn cannot_delete_last_parent() {
    let (app, _auth) = boot_with_parent_and_child(AccountType::Parent).await;
    let parent_id = app.parent_id.unwrap();
    let res = app.server.delete(&format!("/api/accounts/{parent_id}")).await;
    assert_eq!(res.status_code(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn deleting_a_child_succeeds() {
    let (app, _auth) = boot_with_parent_and_child(AccountType::Parent).await;
    let child_id = app.child_id.unwrap();
    let res = app.server.delete(&format!("/api/accounts/{child_id}")).await;
    assert_eq!(res.status_code(), StatusCode::NO_CONTENT);
}

#[tokio::test]
async fn account_update_404_for_missing() {
    let (app, _auth) = boot_with_parent_and_child(AccountType::Parent).await;
    let res = app
        .server
        .put("/api/accounts/9999")
        .json(&json!({ "display_name": "x" }))
        .await;
    assert_eq!(res.status_code(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn account_update_rejects_invalid_role() {
    let (app, _auth) = boot_with_parent_and_child(AccountType::Parent).await;
    let child_id = app.child_id.unwrap();
    let res = app
        .server
        .put(&format!("/api/accounts/{child_id}"))
        .json(&json!({ "account_type": "nonsense" }))
        .await;
    assert_eq!(res.status_code(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn cannot_demote_last_parent_to_child() {
    let (app, _auth) = boot_with_parent_and_child(AccountType::Parent).await;
    let parent_id = app.parent_id.unwrap();
    let res = app
        .server
        .put(&format!("/api/accounts/{parent_id}"))
        .json(&json!({ "account_type": "child" }))
        .await;
    assert_eq!(res.status_code(), StatusCode::BAD_REQUEST);
}

// ---------------------------------------------------------------------------
// Child settings + usage limits.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn child_settings_get_returns_defaults() {
    let (app, _auth) = boot_with_parent_and_child(AccountType::Parent).await;
    let child_id = app.child_id.unwrap();
    let res = app
        .server
        .get(&format!("/api/children/{child_id}/settings"))
        .await;
    assert!(res.status_code().is_success());
    let body: serde_json::Value = res.json();
    assert!(body["downloads_enabled"].as_bool().unwrap_or(false));
}

#[tokio::test]
async fn child_settings_put_persists_changes() {
    let (app, _auth) = boot_with_parent_and_child(AccountType::Parent).await;
    let child_id = app.child_id.unwrap();
    let res = app
        .server
        .put(&format!("/api/children/{child_id}/settings"))
        .json(&json!({
            "downloads_enabled": false,
            "max_quality": "720p",
            "playback_speed_locked": true,
            "autoplay_enabled": true,
        }))
        .await;
    assert!(res.status_code().is_success());
    let body: serde_json::Value = res.json();
    assert_eq!(body["downloads_enabled"].as_bool(), Some(false));
    assert_eq!(body["max_quality"], "720p");
    assert_eq!(body["playback_speed_locked"].as_bool(), Some(true));
}

#[tokio::test]
async fn child_settings_rejects_bad_max_quality() {
    let (app, _auth) = boot_with_parent_and_child(AccountType::Parent).await;
    let child_id = app.child_id.unwrap();
    let res = app
        .server
        .put(&format!("/api/children/{child_id}/settings"))
        .json(&json!({ "max_quality": "potato" }))
        .await;
    assert_eq!(res.status_code(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn usage_limits_round_trip() {
    let (app, _auth) = boot_with_parent_and_child(AccountType::Parent).await;
    let child_id = app.child_id.unwrap();

    // Empty initially.
    let res = app
        .server
        .get(&format!("/api/children/{child_id}/usage-limits"))
        .await;
    let body: serde_json::Value = res.json();
    assert!(body.as_array().unwrap().is_empty());

    // Set Mon-Fri.
    let limits: Vec<serde_json::Value> = (1..=5)
        .map(|d| json!({
            "day_of_week": d,
            "max_hours": 2.0,
            "allowed_start_time": "08:00",
            "allowed_end_time": "20:00",
        }))
        .collect();
    let res = app
        .server
        .put(&format!("/api/children/{child_id}/usage-limits"))
        .json(&limits)
        .await;
    assert!(res.status_code().is_success());
    let body: serde_json::Value = res.json();
    assert_eq!(body.as_array().unwrap().len(), 5);
}

#[tokio::test]
async fn usage_limits_rejects_invalid_day() {
    let (app, _auth) = boot_with_parent_and_child(AccountType::Parent).await;
    let child_id = app.child_id.unwrap();
    let res = app
        .server
        .put(&format!("/api/children/{child_id}/usage-limits"))
        .json(&json!([
            { "day_of_week": 99, "max_hours": 1.0, "allowed_start_time": "08:00", "allowed_end_time": "20:00" }
        ]))
        .await;
    assert_eq!(res.status_code(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn usage_stats_returns_zero_for_fresh_child() {
    let (app, _auth) = boot_with_parent_and_child(AccountType::Parent).await;
    let child_id = app.child_id.unwrap();
    let res = app
        .server
        .get(&format!("/api/children/{child_id}/usage-stats"))
        .await;
    assert!(res.status_code().is_success());
    let body: serde_json::Value = res.json();
    // The schema includes `today` + `weekly` arrays.
    assert!(body["weekly"].is_array());
}

// ---------------------------------------------------------------------------
// Blocked videos.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn blocked_list_for_non_child_400() {
    let (app, _auth) = boot_with_parent_and_child(AccountType::Parent).await;
    let parent_id = app.parent_id.unwrap();
    let res = app
        .server
        .get(&format!("/api/children/{parent_id}/blocked"))
        .await;
    assert_eq!(res.status_code(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn allowlist_list_empty_for_fresh_child() {
    let (app, _auth) = boot_with_parent_and_child(AccountType::Parent).await;
    let child_id = app.child_id.unwrap();
    for kind in ["videos", "channels", "playlists"] {
        let res = app
            .server
            .get(&format!("/api/children/{child_id}/allowlist/{kind}"))
            .await;
        assert!(res.status_code().is_success());
        let body: serde_json::Value = res.json();
        assert!(body.as_array().unwrap().is_empty());
    }
}

#[tokio::test]
async fn cron_run_now_with_scheduler_none_500() {
    let (app, _auth) = boot_with_parent_and_child(AccountType::Parent).await;
    hometube::services::cron::seed_default_jobs(&app.pool)
        .await
        .unwrap();
    let id: i64 = sqlx::query_scalar("SELECT id FROM cron_jobs LIMIT 1")
        .fetch_one(&app.pool)
        .await
        .unwrap();
    // The harness boots without a scheduler installed.
    let res = app.server.post(&format!("/api/cron/jobs/{id}/run")).await;
    assert_eq!(res.status_code(), StatusCode::INTERNAL_SERVER_ERROR);
}

#[tokio::test]
async fn cron_get_404_for_unknown_job() {
    let (app, _auth) = boot_with_parent_and_child(AccountType::Parent).await;
    let res = app.server.get("/api/cron/jobs/9999").await;
    assert_eq!(res.status_code(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn cron_list_runs_returns_array() {
    let (app, _auth) = boot_with_parent_and_child(AccountType::Parent).await;
    hometube::services::cron::seed_default_jobs(&app.pool)
        .await
        .unwrap();
    let id: i64 = sqlx::query_scalar("SELECT id FROM cron_jobs LIMIT 1")
        .fetch_one(&app.pool)
        .await
        .unwrap();

    // Insert a fake run row.
    sqlx::query(
        "INSERT INTO cron_job_runs (job_id, started_at, status, message) \
         VALUES (?, unixepoch(), 'success', 'test')",
    )
    .bind(id)
    .execute(&app.pool)
    .await
    .unwrap();

    let res = app
        .server
        .get(&format!("/api/cron/jobs/{id}/runs"))
        .await;
    assert!(res.status_code().is_success());
    let body: serde_json::Value = res.json();
    assert_eq!(body[0]["job_id"], id);
}

#[tokio::test]
async fn cache_videos_list_with_seeded_segment() {
    let (app, _auth) = boot_with_parent_and_child(AccountType::Parent).await;
    sqlx::query(
        "INSERT INTO segment_cache (video_id, format_id, segment_number, file_path, file_size_bytes) \
         VALUES ('vid-1', '137', 0, '/tmp/fake', 1234)",
    )
    .execute(&app.pool)
    .await
    .unwrap();

    let res = app.server.get("/api/cache/videos").await;
    assert!(res.status_code().is_success());
    let body: serde_json::Value = res.json();
    assert_eq!(body[0]["video_id"], "vid-1");

    // Stats now reflects the seeded segment.
    let res = app.server.get("/api/cache/stats").await;
    let body: serde_json::Value = res.json();
    assert!(body["total_bytes"].as_i64().unwrap() >= 1234);
    assert!(body["segment_count"].as_i64().unwrap() >= 1);
}

#[tokio::test]
async fn cache_delete_video_path() {
    let (app, _auth) = boot_with_parent_and_child(AccountType::Parent).await;
    sqlx::query(
        "INSERT INTO segment_cache (video_id, format_id, segment_number, file_path, file_size_bytes) \
         VALUES ('vid-2', '137', 0, '/tmp/fake-2', 1234)",
    )
    .execute(&app.pool)
    .await
    .unwrap();

    let res = app.server.delete("/api/cache/videos/vid-2").await;
    assert_eq!(res.status_code(), StatusCode::NO_CONTENT);

    let count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM segment_cache WHERE video_id = 'vid-2'")
            .fetch_one(&app.pool)
            .await
            .unwrap();
    assert_eq!(count, 0);
}

#[tokio::test]
async fn blocked_videos_round_trip_via_db_seed() {
    let (app, _auth) = boot_with_parent_and_child(AccountType::Parent).await;
    let child_id = app.child_id.unwrap();
    let parent_id = app.parent_id.unwrap();

    sqlx::query(
        "INSERT INTO blocked_videos (child_account_id, video_id, video_title, blocked_by) \
         VALUES (?, 'vid-bad', 'Bad', ?)",
    )
    .bind(child_id)
    .bind(parent_id)
    .execute(&app.pool)
    .await
    .unwrap();

    let res = app
        .server
        .get(&format!("/api/children/{child_id}/blocked"))
        .await;
    assert_eq!(res.status_code(), StatusCode::OK);
    let body: serde_json::Value = res.json();
    assert_eq!(body[0]["video_id"], "vid-bad");

    let res = app
        .server
        .delete(&format!("/api/children/{child_id}/blocked/vid-bad"))
        .await;
    assert_eq!(res.status_code(), StatusCode::NO_CONTENT);
}

// ---------------------------------------------------------------------------
// Family.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn family_members_returns_both_accounts() {
    let (app, _auth) = boot_with_parent_and_child(AccountType::Parent).await;
    let res = app.server.get("/api/family/members").await;
    assert_eq!(res.status_code(), StatusCode::OK);
    let body: serde_json::Value = res.json();
    let arr = body.as_array().unwrap();
    assert_eq!(arr.len(), 2);
    let types: Vec<&str> = arr.iter().map(|m| m["account_type"].as_str().unwrap()).collect();
    assert!(types.contains(&"parent"));
    assert!(types.contains(&"child"));
}

#[tokio::test]
async fn family_update_member_renames() {
    let (app, _auth) = boot_with_parent_and_child(AccountType::Parent).await;
    let child_id = app.child_id.unwrap();
    let res = app
        .server
        .put(&format!("/api/family/members/{child_id}"))
        .json(&json!({ "display_name": "New Name" }))
        .await;
    assert!(res.status_code().is_success());
}

#[tokio::test]
async fn family_add_member_returns_login_url() {
    let (app, _auth) = boot_with_parent_and_child(AccountType::Parent).await;
    let res = app
        .server
        .post("/api/family/members")
        .json(&json!({ "role": "child", "display_name": "Kiddo" }))
        .await;
    assert!(res.status_code().is_success());
    let body: serde_json::Value = res.json();
    let url = body["login_url"].as_str().unwrap();
    assert!(url.contains("role=child"));
    assert!(url.contains("context=add_member"));
}

#[tokio::test]
async fn family_add_member_rejects_invalid_role() {
    let (app, _auth) = boot_with_parent_and_child(AccountType::Parent).await;
    let res = app
        .server
        .post("/api/family/members")
        .json(&json!({ "role": "alien", "display_name": null }))
        .await;
    assert_eq!(res.status_code(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn family_update_404_for_missing_id() {
    let (app, _auth) = boot_with_parent_and_child(AccountType::Parent).await;
    let res = app
        .server
        .put("/api/family/members/99999")
        .json(&json!({ "display_name": "x" }))
        .await;
    assert_eq!(res.status_code(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn family_update_rejects_demote_only_parent() {
    let (app, _auth) = boot_with_parent_and_child(AccountType::Parent).await;
    let parent_id = app.parent_id.unwrap();
    let res = app
        .server
        .put(&format!("/api/family/members/{parent_id}"))
        .json(&json!({ "role": "child" }))
        .await;
    assert_eq!(res.status_code(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn family_delete_refuses_last_parent() {
    let (app, _auth) = boot_with_parent_and_child(AccountType::Parent).await;
    let parent_id = app.parent_id.unwrap();
    let res = app
        .server
        .delete(&format!("/api/family/members/{parent_id}"))
        .await;
    assert_eq!(res.status_code(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn family_delete_404_for_missing_id() {
    let (app, _auth) = boot_with_parent_and_child(AccountType::Parent).await;
    let res = app.server.delete("/api/family/members/9999").await;
    assert_eq!(res.status_code(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn family_delete_child_succeeds() {
    let (app, _auth) = boot_with_parent_and_child(AccountType::Parent).await;
    let child_id = app.child_id.unwrap();
    let res = app.server.delete(&format!("/api/family/members/{child_id}")).await;
    assert_eq!(res.status_code(), StatusCode::NO_CONTENT);
}

#[tokio::test]
async fn family_reauth_returns_login_url() {
    let (app, _auth) = boot_with_parent_and_child(AccountType::Parent).await;
    let parent_id = app.parent_id.unwrap();
    let res = app
        .server
        .post(&format!("/api/family/members/{parent_id}/reauth"))
        .await;
    assert!(res.status_code().is_success());
    let body: serde_json::Value = res.json();
    assert!(body["login_url"].as_str().unwrap().contains("context=reauth"));
}

#[tokio::test]
async fn family_reauth_404_for_missing() {
    let (app, _auth) = boot_with_parent_and_child(AccountType::Parent).await;
    let res = app.server.post("/api/family/members/9999/reauth").await;
    assert_eq!(res.status_code(), StatusCode::NOT_FOUND);
}

// ---------------------------------------------------------------------------
// System (yt-dlp).
// ---------------------------------------------------------------------------

#[tokio::test]
async fn system_ytdlp_returns_status() {
    let (app, _auth) = boot_with_parent_and_child(AccountType::Parent).await;
    // Seed the singleton row so the handler has something to return.
    sqlx::query(
        "INSERT INTO ytdlp_info (id, current_version, binary_path) \
         VALUES (1, '2024.01.01', 'yt-dlp')",
    )
    .execute(&app.pool)
    .await
    .unwrap();
    let res = app.server.get("/api/system/ytdlp").await;
    assert_eq!(res.status_code(), StatusCode::OK);
    let body: serde_json::Value = res.json();
    assert_eq!(body["binary_path"], "yt-dlp");
}

#[tokio::test]
async fn system_ytdlp_update_500_without_scheduler() {
    let (app, _auth) = boot_with_parent_and_child(AccountType::Parent).await;
    // No scheduler installed → handler returns 500.
    let res = app.server.post("/api/system/ytdlp/update").await;
    assert_eq!(res.status_code(), StatusCode::INTERNAL_SERVER_ERROR);
}

// ---------------------------------------------------------------------------
// Activity dashboard.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn activity_summary_works_for_fresh_child() {
    let (app, _auth) = boot_with_parent_and_child(AccountType::Parent).await;
    let child_id = app.child_id.unwrap();
    let res = app
        .server
        .get(&format!("/api/children/{child_id}/activity/summary"))
        .await;
    assert!(res.status_code().is_success());
}

#[tokio::test]
async fn activity_history_works_for_fresh_child() {
    let (app, _auth) = boot_with_parent_and_child(AccountType::Parent).await;
    let child_id = app.child_id.unwrap();
    let res = app
        .server
        .get(&format!("/api/children/{child_id}/activity/history"))
        .await;
    assert!(res.status_code().is_success());
}

#[tokio::test]
async fn activity_top_channels_works_for_fresh_child() {
    let (app, _auth) = boot_with_parent_and_child(AccountType::Parent).await;
    let child_id = app.child_id.unwrap();
    let res = app
        .server
        .get(&format!("/api/children/{child_id}/activity/top-channels"))
        .await;
    assert!(res.status_code().is_success());
}

#[tokio::test]
async fn activity_search_log_works_for_fresh_child() {
    let (app, _auth) = boot_with_parent_and_child(AccountType::Parent).await;
    let child_id = app.child_id.unwrap();
    let res = app
        .server
        .get(&format!("/api/children/{child_id}/activity/search-log"))
        .await;
    assert!(res.status_code().is_success());
}
