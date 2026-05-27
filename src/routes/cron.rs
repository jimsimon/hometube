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
    // `cron_jobs_with_last_run` is a view introduced by migration 026
    // that left-joins `cron_jobs` with its most recent finalised
    // `cron_job_runs` row, producing the same shape that the legacy
    // `cron_jobs.last_run_*` columns used to expose.
    let rows: Vec<CronJobRow> = sqlx::query_as(
        "SELECT id, name, description, job_type, schedule, schedule_preset, \
                allowed_presets, enabled, last_run_at, last_run_status, \
                last_run_message, next_run_at \
         FROM cron_jobs_with_last_run ORDER BY name",
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
         FROM cron_jobs_with_last_run WHERE id = ?",
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
    // Reject empty bodies up front. A PUT with no recognised fields
    // would otherwise round-trip an empty transaction and silently
    // return the current row — indistinguishable from a successful
    // update from the client's perspective. This is a "did you mean
    // anything?" gate, not a no-op short-circuit: a PUT that supplies
    // `enabled` equal to its already-stored value still takes the
    // full transaction + scheduler-reregister path (which is
    // idempotent and cheap).
    if body.enabled.is_none() && body.schedule_preset.is_none() {
        return Err(AppError::BadRequest(
            "request body must include 'enabled' and/or 'schedule_preset'".into(),
        ));
    }

    // Wrap both possible user-driven writes (enabled, schedule_preset)
    // in a single transaction so they commit atomically — a reader
    // querying *this connection's* writes can't see "enabled flipped
    // but schedule still on the old preset". (SQLite's default
    // read-committed semantics mean readers on *other* pooled
    // connections still see the writes atomically at commit time.)
    // The preset-validation SELECT also runs on the same tx
    // connection so it sees any in-flight `enabled` write.
    //
    // Use `BEGIN IMMEDIATE` (instead of the default `BEGIN DEFERRED`)
    // so the transaction acquires a write lock up-front. With deferred
    // begin, two concurrent PUTs against the same id can both pass
    // the existence-check SELECT (a deferred read takes no lock) and
    // then race for the write lock — SQLite would still serialise the
    // writes, but the second writer's snapshot is stale and a future
    // edit relying on read-your-snapshot semantics would silently
    // break. IMMEDIATE serialises us at `begin_with` time, so two
    // concurrent PUTs queue cleanly and both observe the other's
    // committed state.
    let mut tx = state.db.begin_with("BEGIN IMMEDIATE").await?;

    // Existence precondition: short-circuit a 404 before we run
    // `register_all` (which iterates every job in the DB) just to
    // discover that the row didn't exist. With `BEGIN IMMEDIATE` in
    // place the snapshot here is also the snapshot the writes below
    // see, so concurrent admins can't (e.g.) delete the row between
    // the check and the UPDATE.
    let exists: Option<i64> = sqlx::query_scalar("SELECT id FROM cron_jobs WHERE id = ?")
        .bind(id)
        .fetch_optional(&mut *tx)
        .await?;
    if exists.is_none() {
        return Err(AppError::NotFound);
    }

    if let Some(enabled) = body.enabled {
        sqlx::query("UPDATE cron_jobs SET enabled = ? WHERE id = ?")
            .bind(enabled as i64)
            .bind(id)
            .execute(&mut *tx)
            .await?;
    }
    if let Some(preset) = body.schedule_preset {
        // Validate preset is allowed for this job.
        let allowed_json: String =
            sqlx::query_scalar("SELECT allowed_presets FROM cron_jobs WHERE id = ?")
                .bind(id)
                .fetch_optional(&mut *tx)
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
            .execute(&mut *tx)
            .await?;
    }
    tx.commit().await?;

    // Re-register all jobs with the scheduler. The simplest thing that
    // works: drop the in-memory schedule and rebuild from the DB. The
    // existing scheduler doesn't expose a per-job remove API on the
    // version we depend on, so the scheduler implementation handles
    // duplicate registration gracefully. `register_all` also refreshes
    // every job's `next_run_at`; for the disabled-job branch we call
    // `refresh_next_run_at_for_all` explicitly so the UI shows `null`.
    //
    // Both await fully before we re-fetch so the response sees the
    // freshly-written `next_run_at`. We're on a shared `SqlitePool`,
    // so post-commit reads from any pooled connection are guaranteed
    // to see the writes (WAL).
    if let Some(sched) = state.scheduler.as_ref() {
        if let Err(err) = sched.register_all().await {
            tracing::warn!(%err, "re-registering cron jobs after update");
        }
        if let Err(err) = sched.refresh_next_run_at_for_all().await {
            tracing::warn!(%err, "refreshing next_run_at after update");
        }
    }

    // Re-fetch the row. `fetch_optional` (not `fetch_one`) handles
    // the (currently impossible but cheap-to-defend-against) case
    // where the row disappears between `tx.commit()` and this SELECT.
    //
    // The earlier existence check inside `tx` guards against
    // UPDATE-ing zero rows — its `tx` snapshot is gone by the time we
    // get here, so it can't make any guarantees about post-commit
    // visibility. Today no code path DELETEs from `cron_jobs` (rows
    // are seeded by migrations only), so the only realistic way this
    // SELECT returns `None` is a future admin/migration path adding
    // such a DELETE — in which case the `Option` shape here already
    // converts it to a clean 404 instead of sqlx bubbling
    // `RowNotFound` as a 500.
    let row: Option<CronJobRow> = sqlx::query_as(
        "SELECT id, name, description, job_type, schedule, schedule_preset, \
                allowed_presets, enabled, last_run_at, last_run_status, \
                last_run_message, next_run_at \
         FROM cron_jobs_with_last_run WHERE id = ?",
    )
    .bind(id)
    .fetch_optional(&state.db)
    .await?;
    let row = row.ok_or(AppError::NotFound)?;
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
