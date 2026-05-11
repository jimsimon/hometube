//! Cron-job CRUD coverage.
//!
//! After [`hometube::services::cron::seed_default_jobs`] runs, the
//! `cron_jobs` table holds the three jobs documented in the plan:
//! `ytdlp_update`, `youtube_sync`, and `cache_cleanup`. We assert their
//! presence, the preset → cron-expression mapping behind
//! `PUT /api/cron/jobs/:id`, and the validation path for unsupported
//! presets.
//!
//! `next_run_at` is populated by the live scheduler in production but
//! computed deterministically by [`hometube::services::cron::compute_next_run_at`]
//! against the cron expression — we use that helper in our assertion so
//! the test doesn't depend on the in-process scheduler being started.

mod common;

use axum::http::StatusCode;
use common::boot_with_parent_and_child;
use hometube::models::account::AccountType;
use hometube::services::cron::{compute_next_run_at, seed_default_jobs};
use serde_json::json;

async fn boot_with_cron_seeded() -> common::TestApp {
    let (app, _auth) = boot_with_parent_and_child(AccountType::Parent).await;
    seed_default_jobs(&app.pool).await.expect("seed cron jobs");
    // Mirror what the production startup does: persist next_run_at for
    // every enabled job so the API has a value to return.
    let rows: Vec<(i64, String)> =
        sqlx::query_as("SELECT id, schedule FROM cron_jobs WHERE enabled = 1")
            .fetch_all(&app.pool)
            .await
            .unwrap();
    for (id, expr) in rows {
        if let Some(next) = compute_next_run_at(&expr) {
            sqlx::query("UPDATE cron_jobs SET next_run_at = ? WHERE id = ?")
                .bind(next)
                .bind(id)
                .execute(&app.pool)
                .await
                .unwrap();
        }
    }
    app
}

#[tokio::test]
async fn default_jobs_are_seeded() {
    let app = boot_with_cron_seeded().await;
    let res = app.server.get("/api/cron/jobs").await;
    assert_eq!(res.status_code(), StatusCode::OK);
    let body: serde_json::Value = res.json();
    let names: Vec<String> = body
        .as_array()
        .unwrap()
        .iter()
        .map(|j| j["name"].as_str().unwrap().to_string())
        .collect();
    assert!(names.contains(&"ytdlp_update".to_string()));
    assert!(names.contains(&"youtube_sync".to_string()));
    assert!(names.contains(&"cache_cleanup".to_string()));
}

#[tokio::test]
async fn list_jobs_includes_next_run_at() {
    let app = boot_with_cron_seeded().await;
    let res = app.server.get("/api/cron/jobs").await;
    let body: serde_json::Value = res.json();
    for job in body.as_array().unwrap() {
        let next = job["next_run_at"].as_i64().expect("next_run_at populated");
        assert!(
            next > 0,
            "next_run_at should be a positive unix timestamp (got {next})"
        );
    }
}

#[tokio::test]
async fn put_disable_flips_enabled() {
    let app = boot_with_cron_seeded().await;
    // Pull an arbitrary job ID from the list.
    let res = app.server.get("/api/cron/jobs").await;
    let body: serde_json::Value = res.json();
    let id = body[0]["id"].as_i64().unwrap();

    let res = app
        .server
        .put(&format!("/api/cron/jobs/{id}"))
        .json(&json!({ "enabled": false }))
        .await;
    assert_eq!(res.status_code(), StatusCode::OK);
    let body: serde_json::Value = res.json();
    assert_eq!(body["enabled"], false);
}

#[tokio::test]
async fn put_known_preset_updates_schedule() {
    let app = boot_with_cron_seeded().await;
    // youtube_sync's allowed_presets includes "Every hour".
    let id: i64 = sqlx::query_scalar("SELECT id FROM cron_jobs WHERE name = 'youtube_sync'")
        .fetch_one(&app.pool)
        .await
        .unwrap();
    let res = app
        .server
        .put(&format!("/api/cron/jobs/{id}"))
        .json(&json!({ "schedule_preset": "Every hour" }))
        .await;
    assert_eq!(res.status_code(), StatusCode::OK);
    let body: serde_json::Value = res.json();
    assert_eq!(body["schedule"], "0 * * * *");
    assert_eq!(body["schedule_preset"], "Every hour");
}

#[tokio::test]
async fn put_unsupported_preset_is_400() {
    let app = boot_with_cron_seeded().await;
    let id: i64 = sqlx::query_scalar("SELECT id FROM cron_jobs WHERE name = 'ytdlp_update'")
        .fetch_one(&app.pool)
        .await
        .unwrap();
    // ytdlp_update doesn't allow "Every 15 minutes".
    let res = app
        .server
        .put(&format!("/api/cron/jobs/{id}"))
        .json(&json!({ "schedule_preset": "Every 15 minutes" }))
        .await;
    assert_eq!(res.status_code(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn get_single_job_includes_runs_array() {
    let app = boot_with_cron_seeded().await;
    let id: i64 = sqlx::query_scalar("SELECT id FROM cron_jobs LIMIT 1")
        .fetch_one(&app.pool)
        .await
        .unwrap();
    let res = app.server.get(&format!("/api/cron/jobs/{id}")).await;
    assert_eq!(res.status_code(), StatusCode::OK);
    let body: serde_json::Value = res.json();
    assert!(body["job"].is_object());
    assert!(body["runs"].is_array());
}
