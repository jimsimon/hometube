-- Derive `cron_jobs.last_run_*` from `cron_job_runs`.
--
-- `cron_jobs` previously carried a "last run summary" (`last_run_at`,
-- `last_run_status`, `last_run_message`) that exactly duplicates the most
-- recent finalized row in `cron_job_runs`. The two are only kept in sync
-- by a single UPDATE in `cron.rs::finalize_run`; a crash between the run-
-- row insert and that UPDATE leaves them drifted.
--
-- Drop the cached columns and replace them with a `cron_jobs_with_last_run`
-- view that the admin UI selects from. `next_run_at` stays on `cron_jobs`
-- because it's a *forward-looking* value computed from the schedule, not
-- a fact about a past run.

COMMIT;
PRAGMA foreign_keys = OFF;
BEGIN;

CREATE TABLE cron_jobs_new (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    name TEXT UNIQUE NOT NULL,
    description TEXT,
    job_type TEXT NOT NULL,
    schedule TEXT NOT NULL,
    schedule_preset TEXT,
    allowed_presets TEXT NOT NULL,
    enabled INTEGER NOT NULL DEFAULT 1,
    next_run_at INTEGER,
    created_at INTEGER NOT NULL DEFAULT (unixepoch())
);
INSERT INTO cron_jobs_new (id, name, description, job_type, schedule, schedule_preset,
                           allowed_presets, enabled, next_run_at, created_at)
SELECT id, name, description, job_type, schedule, schedule_preset,
       allowed_presets, enabled, next_run_at, created_at
  FROM cron_jobs;
DROP TABLE cron_jobs;
ALTER TABLE cron_jobs_new RENAME TO cron_jobs;

-- Per-job "latest finalised run" lookup support.
CREATE INDEX idx_cron_job_runs_job_started
    ON cron_job_runs(job_id, started_at DESC);

-- Convenience view: cron_jobs joined with the most recent terminal run.
--
-- Two correlated subqueries:
--
-- * `r` — most recent *finalised* run (status != 'running'). Powers
--   the `last_run_*` columns the admin UI displays as "last
--   successful/failed run". Orphan `'running'` rows (process crash
--   between dispatch and `finalize_run`) are deliberately hidden here:
--   they don't represent a known outcome, so surfacing them as the
--   "last run" would mislead operators into thinking the job
--   succeeded/failed.
--
-- * `a` — most recent *attempted* run, including `'running'`. Powers
--   `last_attempted_*` so operators can spot orphan rows in the admin
--   UI (`last_attempted_status = 'running'` AND `last_attempted_at` is
--   far in the past ⇒ stuck row, recover by setting
--   `cron_job_runs.status = 'failed'` for that id).
--
-- `id DESC` is the tiebreaker for sub-second collisions in both
-- subqueries (e.g. `run_now` immediately after a scheduled tick):
-- without it SQLite's choice between equal `started_at` rows is
-- implementation-defined and the admin UI can flicker.
CREATE VIEW cron_jobs_with_last_run AS
SELECT j.id,
       j.name,
       j.description,
       j.job_type,
       j.schedule,
       j.schedule_preset,
       j.allowed_presets,
       j.enabled,
       j.next_run_at,
       j.created_at,
       r.started_at  AS last_run_at,
       r.status      AS last_run_status,
       r.message     AS last_run_message,
       a.started_at  AS last_attempted_at,
       a.status      AS last_attempted_status,
       a.message     AS last_attempted_message
  FROM cron_jobs j
  LEFT JOIN cron_job_runs r ON r.id = (
      SELECT id FROM cron_job_runs
       WHERE job_id = j.id AND status != 'running'
       ORDER BY started_at DESC, id DESC
       LIMIT 1
  )
  LEFT JOIN cron_job_runs a ON a.id = (
      SELECT id FROM cron_job_runs
       WHERE job_id = j.id
       ORDER BY started_at DESC, id DESC
       LIMIT 1
  );

COMMIT;

PRAGMA foreign_key_check;

PRAGMA foreign_keys = ON;

BEGIN;
