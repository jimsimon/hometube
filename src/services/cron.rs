//! Cron-job scheduler.
//!
//! HomeTube uses [`tokio_cron_scheduler`] for in-process job scheduling.
//! Job definitions live in the `cron_jobs` table; on startup we load
//! every enabled row and register it with the scheduler. Each job_type
//! maps to a Rust handler function:
//!
//! - `ytdlp_update`   → [`run_ytdlp_update`]
//! - `cache_cleanup`  → [`run_cache_cleanup`]
//!
//! Each invocation:
//!
//! 1. Inserts a `cron_job_runs` row with `status='running'`.
//! 2. Awaits the handler.
//! 3. Updates the run row with `status`, `finished_at`, `message`,
//!    `output` (truncated).
//! 4. Updates the parent `cron_jobs` row's `last_run_*` and
//!    `next_run_at` columns.
//!
//! ## Cron expression mapping
//!
//! The plan's preset expressions are 5-field POSIX-style (`m h dom mon
//! dow`). `tokio-cron-scheduler` expects a 6-field expression with
//! seconds at the front, so we prepend `"0 "` before handing it to the
//! library. This module also exposes [`preset_to_expression`] +
//! [`expression_to_preset`] for the parent UI.

use std::str::FromStr;
use std::sync::Arc;

use chrono::Utc;
use cron::Schedule;
use sqlx::SqlitePool;
use tokio::sync::Mutex;
use tokio_cron_scheduler::{Job, JobScheduler};
use tracing::{debug, error, info, warn};

use crate::config::Config;
use crate::error::AppResult;
use crate::services::ytdlp;

/// Maximum number of bytes of stdout/stderr we persist per run.
const OUTPUT_TRUNCATE_BYTES: usize = 8 * 1024;

/// Names used as primary key in `cron_jobs.name`. The `name` column has
/// a UNIQUE constraint, so reseeding is safe via `INSERT OR IGNORE`.
pub const NAME_YTDLP_UPDATE: &str = "ytdlp_update";
pub const NAME_CACHE_CLEANUP: &str = "cache_cleanup";
pub const NAME_FEED_GC: &str = "feed_gc";

/// Allowed preset → cron expression map. The dropdown in the parent UI
/// pulls labels from each job's `allowed_presets` JSON column; the
/// expression itself is stored back to `cron_jobs.schedule`.
pub fn preset_to_expression(preset: &str) -> Option<&'static str> {
    Some(match preset {
        "Every 15 minutes" => "*/15 * * * *",
        "Every 30 minutes" => "*/30 * * * *",
        "Every hour" => "0 * * * *",
        "Every 2 hours" => "0 */2 * * *",
        "Every 6 hours" => "0 */6 * * *",
        "Every 12 hours" => "0 */12 * * *",
        "Daily (3:00 AM)" => "0 3 * * *",
        "Daily (4:00 AM)" => "0 4 * * *",
        "Daily (midnight)" => "0 0 * * *",
        "Weekly (Sunday 3 AM)" => "0 3 * * 0",
        _ => return None,
    })
}

/// Reverse lookup: best-match preset for a given cron expression.
/// Returns `"Custom"` when the expression doesn't match any preset.
pub fn expression_to_preset(expr: &str) -> String {
    let normalised = expr.trim();
    for label in &[
        "Every 15 minutes",
        "Every 30 minutes",
        "Every hour",
        "Every 2 hours",
        "Every 6 hours",
        "Every 12 hours",
        "Daily (3:00 AM)",
        "Daily (4:00 AM)",
        "Daily (midnight)",
        "Weekly (Sunday 3 AM)",
    ] {
        if preset_to_expression(label) == Some(normalised) {
            return (*label).to_string();
        }
    }
    "Custom".to_string()
}

/// Convert a 5-field POSIX cron expression to the 6-field form
/// `tokio-cron-scheduler` expects (seconds at the front).
pub fn to_six_field(expr: &str) -> String {
    let count = expr.split_whitespace().count();
    if count == 6 {
        expr.to_string()
    } else {
        format!("0 {expr}")
    }
}

/// Compute the next firing time of a cron expression as a unix
/// timestamp. Accepts either the 5-field POSIX form or the 6-field form
/// with leading seconds; we normalise to 6-field for the [`cron`]
/// crate's parser.
///
/// Returns `None` if the expression doesn't parse or has no upcoming
/// occurrence.
pub fn compute_next_run_at(expression: &str) -> Option<i64> {
    let six = to_six_field(expression);
    let schedule = Schedule::from_str(&six).ok()?;
    schedule.upcoming(Utc).next().map(|dt| dt.timestamp())
}

/// Persist the computed `next_run_at` for a job. Logs (and swallows)
/// errors — the scheduler must keep running even if this update fails.
async fn update_next_run_at(pool: &SqlitePool, job_id: i64, expression: &str) {
    let next = compute_next_run_at(expression);
    if next.is_none() {
        debug!(
            %expression,
            "could not compute next_run_at for cron expression"
        );
    }
    if let Err(err) = sqlx::query("UPDATE cron_jobs SET next_run_at = ? WHERE id = ?")
        .bind(next)
        .bind(job_id)
        .execute(pool)
        .await
    {
        warn!(%job_id, %err, "updating next_run_at");
    }
}

/// Wrapper around [`JobScheduler`] that owns the database pool used by
/// every handler. Cheap to clone (`Arc`).
#[derive(Clone)]
pub struct Scheduler {
    inner: Arc<Mutex<JobScheduler>>,
    pool: SqlitePool,
    cfg: Config,
}

impl Scheduler {
    /// Build a new scheduler. Does **not** start it — call [`Self::start`]
    /// after registering jobs.
    pub async fn new(pool: SqlitePool, cfg: Config) -> AppResult<Self> {
        let sched = JobScheduler::new().await.map_err(|e| {
            crate::error::AppError::Other(anyhow::anyhow!("creating scheduler: {e}"))
        })?;
        Ok(Self {
            inner: Arc::new(Mutex::new(sched)),
            pool,
            cfg,
        })
    }

    /// Start the scheduler. Idempotent — calling twice is a no-op
    /// (the underlying library tolerates it).
    pub async fn start(&self) -> AppResult<()> {
        let sched = self.inner.lock().await;
        sched.start().await.map_err(|e| {
            crate::error::AppError::Other(anyhow::anyhow!("starting scheduler: {e}"))
        })?;
        Ok(())
    }

    /// Load all enabled jobs from the database and register them. Errors
    /// for a single job are logged and skipped — the scheduler always
    /// starts even if a malformed expression slipped in.
    pub async fn register_all(&self) -> AppResult<()> {
        let rows: Vec<(i64, String, String, String, i64)> =
            sqlx::query_as("SELECT id, name, job_type, schedule, enabled FROM cron_jobs")
                .fetch_all(&self.pool)
                .await?;

        for (job_id, name, job_type, schedule, enabled) in rows {
            if enabled == 0 {
                continue;
            }
            if let Err(err) = self.register_job(job_id, &name, &job_type, &schedule).await {
                warn!(%job_id, %name, %err, "failed to register cron job");
            }
        }
        Ok(())
    }

    /// Register a single job with the scheduler.
    async fn register_job(
        &self,
        job_id: i64,
        name: &str,
        job_type: &str,
        schedule: &str,
    ) -> AppResult<()> {
        let pool = self.pool.clone();
        let cfg = self.cfg.clone();
        let job_type_owned = job_type.to_string();
        let name_owned = name.to_string();
        let expression = to_six_field(schedule);

        let job = Job::new_async(expression.as_str(), move |_uuid, _l| {
            let pool = pool.clone();
            let cfg = cfg.clone();
            let job_type = job_type_owned.clone();
            let name = name_owned.clone();
            Box::pin(async move {
                if let Err(err) = run_job(&pool, &cfg, job_id, &name, &job_type).await {
                    error!(%job_id, %name, %err, "cron job execution failed");
                }
            })
        })
        .map_err(|e| crate::error::AppError::Other(anyhow::anyhow!("building job: {e}")))?;

        {
            let sched = self.inner.lock().await;
            sched
                .add(job)
                .await
                .map_err(|e| crate::error::AppError::Other(anyhow::anyhow!("adding job: {e}")))?;
        }
        // Persist the next firing time so the parent UI's "Next run"
        // column has something useful to display.
        update_next_run_at(&self.pool, job_id, schedule).await;
        info!(%job_id, %name, "registered cron job");
        Ok(())
    }

    /// Recompute and persist `next_run_at` for every enabled job. Used
    /// after the API mutates a job's schedule (re-registration is
    /// handled by [`Self::register_all`]).
    pub async fn refresh_next_run_at_for_all(&self) -> AppResult<()> {
        let rows: Vec<(i64, String, i64)> =
            sqlx::query_as("SELECT id, schedule, enabled FROM cron_jobs")
                .fetch_all(&self.pool)
                .await?;
        for (job_id, schedule, enabled) in rows {
            if enabled == 0 {
                // Still clear next_run_at so the UI doesn't show a stale
                // value for a disabled job.
                let _ = sqlx::query("UPDATE cron_jobs SET next_run_at = NULL WHERE id = ?")
                    .bind(job_id)
                    .execute(&self.pool)
                    .await;
                continue;
            }
            update_next_run_at(&self.pool, job_id, &schedule).await;
        }
        Ok(())
    }

    /// Trigger a single job immediately, off the scheduler. Returns the
    /// `cron_job_runs.id` of the new run row.
    pub async fn run_now(&self, job_id: i64) -> AppResult<i64> {
        let row: Option<(String, String)> =
            sqlx::query_as("SELECT name, job_type FROM cron_jobs WHERE id = ?")
                .bind(job_id)
                .fetch_optional(&self.pool)
                .await?;
        let (name, job_type) = row.ok_or_else(|| crate::error::AppError::NotFound)?;

        // Insert the run row up front so the API can return its ID
        // before the (possibly long-running) handler completes.
        let run_id: i64 = sqlx::query_scalar(
            "INSERT INTO cron_job_runs (job_id, started_at, status) \
             VALUES (?, unixepoch(), 'running') RETURNING id",
        )
        .bind(job_id)
        .fetch_one(&self.pool)
        .await?;

        let pool = self.pool.clone();
        let cfg = self.cfg.clone();
        tokio::spawn(async move {
            let outcome = dispatch(&pool, &cfg, &job_type).await;
            let _ = finalize_run(&pool, run_id, job_id, &name, &outcome).await;
        });

        Ok(run_id)
    }
}

/// Insert a `cron_job_runs` row, run the handler, and persist the
/// outcome.
async fn run_job(
    pool: &SqlitePool,
    cfg: &Config,
    job_id: i64,
    name: &str,
    job_type: &str,
) -> AppResult<()> {
    let run_id: i64 = sqlx::query_scalar(
        "INSERT INTO cron_job_runs (job_id, started_at, status) \
         VALUES (?, unixepoch(), 'running') RETURNING id",
    )
    .bind(job_id)
    .fetch_one(pool)
    .await?;

    let outcome = dispatch(pool, cfg, job_type).await;
    finalize_run(pool, run_id, job_id, name, &outcome).await
}

#[derive(Debug)]
struct RunOutcome {
    success: bool,
    message: String,
    output: String,
}

impl RunOutcome {
    fn success(msg: impl Into<String>) -> Self {
        Self {
            success: true,
            message: msg.into(),
            output: String::new(),
        }
    }
    fn failure(msg: impl Into<String>) -> Self {
        Self {
            success: false,
            message: msg.into(),
            output: String::new(),
        }
    }
    fn with_output(mut self, output: impl Into<String>) -> Self {
        let mut s: String = output.into();
        if s.len() > OUTPUT_TRUNCATE_BYTES {
            s.truncate(OUTPUT_TRUNCATE_BYTES);
            s.push_str("\n...[truncated]");
        }
        self.output = s;
        self
    }
}

async fn dispatch(pool: &SqlitePool, cfg: &Config, job_type: &str) -> RunOutcome {
    match job_type {
        "ytdlp_update" => match run_ytdlp_update(pool, cfg).await {
            Ok(msg) => RunOutcome::success(msg),
            Err(err) => RunOutcome::failure(format!("yt-dlp update failed: {err}")),
        },
        "cache_cleanup" => match run_cache_cleanup(pool).await {
            Ok((msg, output)) => RunOutcome::success(msg).with_output(output),
            Err(err) => RunOutcome::failure(format!("cache cleanup failed: {err}")),
        },
        "feed_gc" => match run_feed_gc(pool).await {
            Ok(msg) => RunOutcome::success(msg),
            Err(err) => RunOutcome::failure(format!("feed gc failed: {err}")),
        },
        other => RunOutcome::failure(format!("unknown job_type: {other}")),
    }
}

async fn finalize_run(
    pool: &SqlitePool,
    run_id: i64,
    job_id: i64,
    _name: &str,
    outcome: &RunOutcome,
) -> AppResult<()> {
    let status = if outcome.success {
        "success"
    } else {
        "failure"
    };
    sqlx::query(
        "UPDATE cron_job_runs SET finished_at = unixepoch(), status = ?, \
            message = ?, output = ? WHERE id = ?",
    )
    .bind(status)
    .bind(&outcome.message)
    .bind(&outcome.output)
    .bind(run_id)
    .execute(pool)
    .await?;

    sqlx::query(
        "UPDATE cron_jobs SET last_run_at = unixepoch(), last_run_status = ?, \
            last_run_message = ? WHERE id = ?",
    )
    .bind(status)
    .bind(&outcome.message)
    .bind(job_id)
    .execute(pool)
    .await?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Job handlers
// ---------------------------------------------------------------------------

async fn run_ytdlp_update(pool: &SqlitePool, cfg: &Config) -> AppResult<String> {
    // [`ytdlp::update_binary`] short-circuits on "already up to date"
    // via [`ytdlp::check_for_update`], so it's cheap to call on every
    // tick. We treat "no new version" + "actually downloaded" as the
    // same successful outcome from the cron's perspective.
    let prior_version: Option<String> =
        sqlx::query_scalar("SELECT current_version FROM ytdlp_info WHERE id = 1")
            .fetch_optional(pool)
            .await?
            .flatten();
    let result = ytdlp::update_binary(pool, cfg).await;
    match result {
        Ok(version) => {
            // Only emit a `system_update` notification when the version
            // actually changed; the no-op "already up to date" path
            // returns the existing version unchanged.
            if prior_version.as_deref() != Some(version.as_str()) {
                let _ = crate::services::notifications::dispatch_ytdlp_upgraded(
                    pool,
                    prior_version.as_deref(),
                    &version,
                )
                .await;
            }
            Ok(format!("yt-dlp updated to {version}"))
        }
        Err(err) => {
            // Notify parents on failure.
            let _ = notify_parents_ytdlp_failure(pool, &err.to_string()).await;
            Err(err)
        }
    }
}

async fn run_cache_cleanup(pool: &SqlitePool) -> AppResult<(String, String)> {
    let (msg, mut detail) = crate::services::video_cache::cleanup_segment_cache(pool).await?;

    // Also evict the thumbnail cache by LRU. The thumbnail cache is a
    // separate disk pool (one file per video) populated by
    // `GET /api/proxy/thumbnail/:videoId` on miss + the backfill
    // prefetch tail-call; its eviction budget is independent of the
    // segment cache's so a busy thumbnail cache can't push out hot
    // DASH segments.
    let max_bytes = crate::services::thumbnail_store::configured_max_bytes(pool).await;
    match crate::services::thumbnail_store::cleanup_lru(pool, max_bytes).await {
        Ok((0, 0)) => {
            detail.push_str("Thumbnail cache under cap; no evictions.\n");
        }
        Ok((evicted, bytes)) => {
            detail.push_str(&format!(
                "Thumbnail cache: evicted {evicted} entries ({} KB) by LRU.\n",
                bytes / 1024
            ));
        }
        Err(err) => {
            tracing::warn!(%err, "thumbnail_cache cleanup failed");
            detail.push_str(&format!("Thumbnail cache cleanup failed: {err}\n"));
        }
    }

    Ok((msg, detail))
}

/// Drop `channel_sync_state` rows (and cascade `channel_videos`) for
/// channels no longer allowlisted by any child. Also calls
/// `channel_backfill::reconcile_with_allowlist` so newly-allowlisted
/// channels missing a sync_state row get one — this catches anything
/// the route-level wiring may have missed (e.g. a direct SQL insert).
async fn run_feed_gc(pool: &SqlitePool) -> AppResult<String> {
    let removed = crate::services::feed_cache::gc_orphan_sources(pool).await?;
    crate::services::channel_backfill::reconcile_with_allowlist(pool).await?;
    Ok(format!("removed {removed} orphan channel(s)"))
}

async fn notify_parents_ytdlp_failure(pool: &SqlitePool, err: &str) -> AppResult<()> {
    let metadata = serde_json::json!({ "error": err });
    crate::services::notifications::broadcast(
        pool,
        crate::services::notifications::TYPE_YTDLP_FAILURE,
        "yt-dlp update failed",
        err,
        &metadata,
    )
    .await
}

// ---------------------------------------------------------------------------
// Default-job seeding
// ---------------------------------------------------------------------------

/// Seed the three default jobs from the plan. Idempotent thanks to the
/// `name` UNIQUE constraint + `INSERT OR IGNORE`.
pub async fn seed_default_jobs(pool: &SqlitePool) -> AppResult<()> {
    let presets_for = |kind: &str| -> Vec<&'static str> {
        match kind {
            "ytdlp_update" => vec![
                "Every 12 hours",
                "Daily (3:00 AM)",
                "Daily (midnight)",
                "Weekly (Sunday 3 AM)",
            ],
            "cache_cleanup" => vec!["Daily (3:00 AM)", "Daily (4:00 AM)"],
            "feed_gc" => vec!["Daily (3:00 AM)", "Daily (4:00 AM)", "Every 12 hours"],
            _ => vec![],
        }
    };

    let defaults: Vec<(
        &'static str,
        &'static str,
        &'static str,
        &'static str,
        &'static str,
    )> = vec![
        (
            NAME_YTDLP_UPDATE,
            "Check for and apply yt-dlp updates from GitHub.",
            "ytdlp_update",
            "0 3 * * *",
            "Daily (3:00 AM)",
        ),
        (
            NAME_CACHE_CLEANUP,
            "Evict cached segments for videos no longer on any allowlist + LRU.",
            "cache_cleanup",
            "0 4 * * *",
            "Daily (4:00 AM)",
        ),
        (
            NAME_FEED_GC,
            "Drop feed_sources rows for channels no longer on any allowlist.",
            "feed_gc",
            "0 3 * * *",
            "Daily (3:00 AM)",
        ),
    ];

    for (name, desc, job_type, schedule, preset) in defaults {
        let presets_json = serde_json::to_string(&presets_for(job_type)).unwrap_or("[]".into());
        sqlx::query(
            "INSERT OR IGNORE INTO cron_jobs \
                (name, description, job_type, schedule, schedule_preset, allowed_presets, enabled) \
             VALUES (?, ?, ?, ?, ?, ?, 1)",
        )
        .bind(name)
        .bind(desc)
        .bind(job_type)
        .bind(schedule)
        .bind(preset)
        .bind(presets_json)
        .execute(pool)
        .await?;
    }
    Ok(())
}

/// Seed the singleton `ytdlp_info` row if it doesn't already exist.
/// Best-effort `--version` lookup so the parent UI shows something
/// useful on first boot.
pub async fn seed_ytdlp_info(pool: &SqlitePool, cfg: &Config) -> AppResult<()> {
    let exists: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM ytdlp_info WHERE id = 1")
        .fetch_one(pool)
        .await?;
    if exists > 0 {
        return Ok(());
    }
    let version = ytdlp::version(cfg).await.ok();
    sqlx::query(
        "INSERT INTO ytdlp_info (id, current_version, last_checked_at, binary_path) \
         VALUES (1, ?, ?, ?)",
    )
    .bind(version)
    .bind(Utc::now().timestamp())
    .bind(&cfg.ytdlp_path)
    .execute(pool)
    .await?;
    Ok(())
}

/// In-process counters for cache hit / miss accounting. Keyed by static
/// strings so they live for the program's lifetime.
pub static CACHE_HIT_COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
pub static CACHE_MISS_COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preset_round_trip_for_every_preset() {
        for label in &[
            "Every 15 minutes",
            "Every 30 minutes",
            "Every hour",
            "Every 2 hours",
            "Every 6 hours",
            "Every 12 hours",
            "Daily (3:00 AM)",
            "Daily (4:00 AM)",
            "Daily (midnight)",
            "Weekly (Sunday 3 AM)",
        ] {
            let expr =
                preset_to_expression(label).unwrap_or_else(|| panic!("known preset {label}"));
            assert_eq!(expression_to_preset(expr), *label);
        }
    }

    #[test]
    fn unknown_preset_round_trips_to_custom() {
        assert!(preset_to_expression("nonsense").is_none());
        assert_eq!(expression_to_preset("0 0 1 1 *"), "Custom");
    }

    #[test]
    fn to_six_field_only_pads_when_needed() {
        assert_eq!(to_six_field("0 * * * *"), "0 0 * * * *");
        assert_eq!(to_six_field("0 0 * * * *"), "0 0 * * * *");
    }

    #[test]
    fn compute_next_run_at_handles_known_expression() {
        let next = compute_next_run_at("0 * * * *").expect("known cron");
        assert!(next > chrono::Utc::now().timestamp());
    }

    #[test]
    fn compute_next_run_at_returns_none_on_garbage() {
        assert!(compute_next_run_at("not a cron").is_none());
    }
}
