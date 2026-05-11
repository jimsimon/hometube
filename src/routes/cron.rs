//! Cron-management API (parent only).
//!
//! Exposes CRUD-style endpoints over the `cron_jobs` and `cron_job_runs`
//! tables, plus a "run now" trigger that defers to
//! [`crate::services::cron::Scheduler::run_now`].
//!
//! All schedule mutations go through preset labels — raw cron
//! expressions are never accepted from the API surface. The preset is
//! validated against the job's `allowed_presets` JSON column.

use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    Json,
};
use serde::{Deserialize, Serialize};

use crate::error::{AppError, AppResult};
use crate::services::cron::{expression_to_preset, preset_to_expression};
use crate::state::AppState;

/// Tuple shape of a row from `cron_jobs` (matched positionally by
/// every `query_as` in this module). Defined as a type alias so the
/// nested types don't trip clippy's `type_complexity` lint.
///
/// Columns in order:
/// `id, name, description, job_type, schedule, schedule_preset,
///  allowed_presets, enabled, last_run_at, last_run_status,
///  last_run_message, next_run_at`.
type CronJobRow = (
    i64,
    String,
    Option<String>,
    String,
    String,
    Option<String>,
    String,
    i64,
    Option<i64>,
    Option<String>,
    Option<String>,
    Option<i64>,
);

#[derive(Debug, Serialize)]
pub struct CronJobView {
    pub id: i64,
    pub name: String,
    pub description: Option<String>,
    pub job_type: String,
    pub schedule: String,
    pub schedule_preset: String,
    pub allowed_presets: Vec<String>,
    pub enabled: bool,
    pub last_run_at: Option<i64>,
    pub last_run_status: Option<String>,
    pub last_run_message: Option<String>,
    pub next_run_at: Option<i64>,
}

#[derive(Debug, Serialize, sqlx::FromRow)]
pub struct CronJobRunView {
    pub id: i64,
    pub job_id: i64,
    pub started_at: i64,
    pub finished_at: Option<i64>,
    pub status: String,
    pub message: Option<String>,
    pub output: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct UpdateJobBody {
    #[serde(default)]
    pub enabled: Option<bool>,
    #[serde(default)]
    pub schedule_preset: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct RunsQuery {
    #[serde(default)]
    pub limit: Option<i64>,
}

/// `GET /api/cron/jobs`.
pub async fn list_jobs(State(state): State<AppState>) -> AppResult<Json<Vec<CronJobView>>> {
    let rows: Vec<CronJobRow> = sqlx::query_as(
        "SELECT id, name, description, job_type, schedule, schedule_preset, \
                allowed_presets, enabled, last_run_at, last_run_status, \
                last_run_message, next_run_at \
         FROM cron_jobs ORDER BY name",
    )
    .fetch_all(&state.db)
    .await?;
    Ok(Json(rows.into_iter().map(into_view).collect()))
}

/// `GET /api/cron/jobs/:id` — single job + its 20 most recent runs.
pub async fn get_job(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> AppResult<Json<serde_json::Value>> {
    let row: Option<CronJobRow> = sqlx::query_as(
        "SELECT id, name, description, job_type, schedule, schedule_preset, \
                allowed_presets, enabled, last_run_at, last_run_status, \
                last_run_message, next_run_at \
         FROM cron_jobs WHERE id = ?",
    )
    .bind(id)
    .fetch_optional(&state.db)
    .await?;
    let row = row.ok_or(AppError::NotFound)?;
    let view = into_view(row);
    let runs: Vec<CronJobRunView> = sqlx::query_as(
        "SELECT id, job_id, started_at, finished_at, status, message, output \
         FROM cron_job_runs WHERE job_id = ? ORDER BY id DESC LIMIT 20",
    )
    .bind(id)
    .fetch_all(&state.db)
    .await?;
    Ok(Json(serde_json::json!({
        "job": view,
        "runs": runs,
    })))
}

/// `PUT /api/cron/jobs/:id`.
pub async fn update_job(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    Json(body): Json<UpdateJobBody>,
) -> AppResult<Json<CronJobView>> {
    if let Some(enabled) = body.enabled {
        sqlx::query("UPDATE cron_jobs SET enabled = ? WHERE id = ?")
            .bind(enabled as i64)
            .bind(id)
            .execute(&state.db)
            .await?;
    }
    if let Some(preset) = body.schedule_preset {
        // Validate preset is allowed for this job.
        let allowed_json: String =
            sqlx::query_scalar("SELECT allowed_presets FROM cron_jobs WHERE id = ?")
                .bind(id)
                .fetch_optional(&state.db)
                .await?
                .ok_or(AppError::NotFound)?;
        let allowed: Vec<String> = serde_json::from_str(&allowed_json).unwrap_or_default();
        if !allowed.contains(&preset) {
            return Err(AppError::BadRequest(format!(
                "preset '{preset}' is not allowed for this job"
            )));
        }
        let expression = preset_to_expression(&preset)
            .ok_or_else(|| AppError::BadRequest(format!("unknown preset '{preset}'")))?;
        sqlx::query("UPDATE cron_jobs SET schedule = ?, schedule_preset = ? WHERE id = ?")
            .bind(expression)
            .bind(&preset)
            .bind(id)
            .execute(&state.db)
            .await?;
    }

    // Re-register all jobs with the scheduler. The simplest thing that
    // works: drop the in-memory schedule and rebuild from the DB. The
    // existing scheduler doesn't expose a per-job remove API on the
    // version we depend on, so the scheduler implementation handles
    // duplicate registration gracefully. `register_all` also refreshes
    // every job's `next_run_at`; for the disabled-job branch we call
    // `refresh_next_run_at_for_all` explicitly so the UI shows `null`.
    if let Some(sched) = state.scheduler.as_ref() {
        if let Err(err) = sched.register_all().await {
            tracing::warn!(%err, "re-registering cron jobs after update");
        }
        if let Err(err) = sched.refresh_next_run_at_for_all().await {
            tracing::warn!(%err, "refreshing next_run_at after update");
        }
    }

    // Re-fetch the row.
    let row: CronJobRow = sqlx::query_as(
        "SELECT id, name, description, job_type, schedule, schedule_preset, \
                allowed_presets, enabled, last_run_at, last_run_status, \
                last_run_message, next_run_at \
         FROM cron_jobs WHERE id = ?",
    )
    .bind(id)
    .fetch_one(&state.db)
    .await?;
    Ok(Json(into_view(row)))
}

#[derive(Debug, Serialize)]
pub struct RunNowResponse {
    pub run_id: i64,
}

/// `POST /api/cron/jobs/:id/run`.
pub async fn run_now(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> AppResult<Json<RunNowResponse>> {
    let sched = state
        .scheduler
        .as_ref()
        .ok_or_else(|| AppError::Other(anyhow::anyhow!("scheduler not initialised")))?;
    let run_id = sched.run_now(id).await?;
    Ok(Json(RunNowResponse { run_id }))
}

/// `GET /api/cron/jobs/:id/runs`.
pub async fn list_runs(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    Query(q): Query<RunsQuery>,
) -> AppResult<Json<Vec<CronJobRunView>>> {
    let limit = q.limit.unwrap_or(20).clamp(1, 100);
    let runs: Vec<CronJobRunView> = sqlx::query_as(
        "SELECT id, job_id, started_at, finished_at, status, message, output \
         FROM cron_job_runs WHERE job_id = ? ORDER BY id DESC LIMIT ?",
    )
    .bind(id)
    .bind(limit)
    .fetch_all(&state.db)
    .await?;
    Ok(Json(runs))
}

fn into_view(row: CronJobRow) -> CronJobView {
    let (
        id,
        name,
        description,
        job_type,
        schedule,
        schedule_preset,
        allowed_presets_json,
        enabled,
        last_run_at,
        last_run_status,
        last_run_message,
        next_run_at,
    ) = row;
    let allowed_presets: Vec<String> =
        serde_json::from_str(&allowed_presets_json).unwrap_or_default();
    let resolved_preset = schedule_preset.unwrap_or_else(|| expression_to_preset(&schedule));
    CronJobView {
        id,
        name,
        description,
        job_type,
        schedule,
        schedule_preset: resolved_preset,
        allowed_presets,
        enabled: enabled != 0,
        last_run_at,
        last_run_status,
        last_run_message,
        next_run_at,
    }
}

/// Used by the router module to map handlers; the trailing `_` is a
/// convenience to silence dead-code warnings.
#[allow(dead_code)]
pub fn _no_content() -> StatusCode {
    StatusCode::NO_CONTENT
}
