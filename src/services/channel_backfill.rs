//! Background task that keeps `channel_videos` warm for the
//! reconciliation tier (yt-dlp `--flat-playlist` per channel).
//!
//! Where the [`feed_refresher`](crate::services::feed_refresher) keeps
//! the *newest* uploads fresh via RSS (and the InnerTube sidecar as a
//! fallback), this loop fills in and periodically reconciles the
//! *entire* upload history per channel. The two run independently —
//! different cadences, different concurrency budgets, different
//! anti-bot pacing — but write into the same `channel_videos` table.
//!
//! ### Concurrency and rate limiting
//!
//! Strictly **single-concurrency family-wide**. At most one yt-dlp
//! subprocess is in flight at any moment. The loop sleeps
//! `min_gap_between_channels` (default 1h, jittered ±15%) between
//! consecutive backfills. Intra-channel InnerTube pagination is
//! throttled by yt-dlp's own `--sleep-*` flags so a single subprocess
//! can't burst-call YouTube either.
//!
//! ### Scheduling
//!
//! After a successful run: `backfill_next_at = now + re_backfill_interval ± 5%`
//! (default 30 days). After a failure: exponential backoff
//! `min(MAX_BACKOFF, channel_interval * 2^errors)`, capped at 24 h. After 5
//! consecutive failures (configurable): set `backfill_status='shelved'`,
//! fire a `channel_backfill_error` notification (24 h-deduped), and
//! stop retrying until the operator clears the shelve via the admin
//! route.
//!
//! ### Lifecycle
//!
//! Spawned from `main` after the freshness refresher. The task loops
//! forever; on shutdown the runtime drops it.

use std::time::Duration;

use rand::RngExt;
use sqlx::SqlitePool;
use tracing::{debug, info, warn};

use crate::config::Config;
use crate::error::AppResult;
use crate::services::ytdlp::{self, FlatPlaylistEntry, FlatPlaylistTunables};

// ---------------------------------------------------------------------------
// Tunable defaults
// ---------------------------------------------------------------------------

/// Default delay between consecutive channel backfills.
pub const DEFAULT_MIN_GAP_BETWEEN_CHANNELS_S: u64 = 60 * 60;

/// Default re-backfill cadence: once per channel per 30 days.
pub const DEFAULT_RE_BACKFILL_INTERVAL_S: u64 = 30 * 24 * 60 * 60;

/// Default per-subprocess timeout.
pub const DEFAULT_SUBPROCESS_TIMEOUT_S: u64 = 30 * 60;

/// yt-dlp `--sleep-requests` value passed to the subprocess.
pub const DEFAULT_YTDLP_SLEEP_REQUESTS_S: u32 = 1;
/// yt-dlp `--sleep-interval` value passed to the subprocess.
pub const DEFAULT_YTDLP_SLEEP_INTERVAL_S: u32 = 1;
/// yt-dlp `--max-sleep-interval` value passed to the subprocess.
pub const DEFAULT_YTDLP_MAX_SLEEP_INTERVAL_S: u32 = 3;

/// How many consecutive failed backfills before we shelve the channel
/// and notify the operator.
pub const DEFAULT_MAX_CONSECUTIVE_ERRORS_BEFORE_SHELVE: i64 = 5;

/// Whether shelving a channel fires a `channel_backfill_error`
/// notification. On by default — the operator wants to know — but
/// flippable via `app_config` so a misbehaving deployment can be
/// silenced quickly.
pub const DEFAULT_NOTIFY_ON_SHELVE: bool = true;

/// Sleep this long when no row is due. Same cadence as the freshness
/// refresher's idle tick for consistency.
pub const DEFAULT_IDLE_TICK_S: u64 = 60;

/// Cap on the exponential backoff between failed attempts.
pub const MAX_BACKOFF: Duration = Duration::from_secs(24 * 60 * 60);

/// How long a claimed channel stays "leased" before another claim can
/// pick it up again. Comfortably larger than the subprocess timeout so
/// the lease only ever expires when the worker has actually crashed
/// without writing back.
pub const LEASE_SECS: i64 = 30 * 60 + 5 * 60;

/// `app_config` keys for the live tunables. Read once per outer-loop
/// iteration; out-of-range values silently fall back to defaults.
pub const KEY_ENABLED: &str = "channel_backfill_enabled";
pub const KEY_MIN_GAP_BETWEEN_CHANNELS_S: &str = "channel_backfill_min_gap_between_channels_s";
pub const KEY_RE_BACKFILL_INTERVAL_S: &str = "channel_backfill_re_backfill_interval_s";
pub const KEY_SUBPROCESS_TIMEOUT_S: &str = "channel_backfill_subprocess_timeout_s";
pub const KEY_YTDLP_SLEEP_REQUESTS_S: &str = "channel_backfill_ytdlp_sleep_requests_s";
pub const KEY_YTDLP_SLEEP_INTERVAL_S: &str = "channel_backfill_ytdlp_sleep_interval_s";
pub const KEY_YTDLP_MAX_SLEEP_INTERVAL_S: &str = "channel_backfill_ytdlp_max_sleep_interval_s";
pub const KEY_MAX_CONSECUTIVE_ERRORS_BEFORE_SHELVE: &str =
    "channel_backfill_max_consecutive_errors_before_shelve";
pub const KEY_NOTIFY_ON_SHELVE: &str = "channel_backfill_notify_on_shelve";
pub const KEY_IDLE_TICK_S: &str = "channel_backfill_idle_tick_s";

// Canonical ranges, used both by `BackfillConfig::load` (to clamp values
// from app_config) and re-exported for the PUT-endpoint validator so the
// two cannot drift apart.
pub const RANGE_MIN_GAP_BETWEEN_CHANNELS_S: std::ops::RangeInclusive<u64> = 300..=86_400;
pub const RANGE_RE_BACKFILL_INTERVAL_S: std::ops::RangeInclusive<u64> = 86_400..=31_536_000;
pub const RANGE_SUBPROCESS_TIMEOUT_S: std::ops::RangeInclusive<u64> = 60..=14_400;
pub const RANGE_YTDLP_SLEEP_S: std::ops::RangeInclusive<u32> = 0..=10;
pub const RANGE_YTDLP_MAX_SLEEP_S: std::ops::RangeInclusive<u32> = 0..=30;
pub const RANGE_MAX_CONSECUTIVE_ERRORS: std::ops::RangeInclusive<i64> = 1..=20;
pub const RANGE_IDLE_TICK_S: std::ops::RangeInclusive<u64> = 5..=3600;

/// A snapshot of the live backfill knobs, taken once per outer-loop
/// iteration. Bad/missing values silently fall back to the defaults.
#[derive(Debug, Clone, Copy)]
pub struct BackfillConfig {
    pub enabled: bool,
    pub min_gap_between_channels: Duration,
    pub re_backfill_interval: Duration,
    pub subprocess_timeout: Duration,
    pub ytdlp_sleep_requests_s: u32,
    pub ytdlp_sleep_interval_s: u32,
    pub ytdlp_max_sleep_interval_s: u32,
    pub max_consecutive_errors_before_shelve: i64,
    pub notify_on_shelve: bool,
    pub idle_tick: Duration,
}

impl Default for BackfillConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            min_gap_between_channels: Duration::from_secs(DEFAULT_MIN_GAP_BETWEEN_CHANNELS_S),
            re_backfill_interval: Duration::from_secs(DEFAULT_RE_BACKFILL_INTERVAL_S),
            subprocess_timeout: Duration::from_secs(DEFAULT_SUBPROCESS_TIMEOUT_S),
            ytdlp_sleep_requests_s: DEFAULT_YTDLP_SLEEP_REQUESTS_S,
            ytdlp_sleep_interval_s: DEFAULT_YTDLP_SLEEP_INTERVAL_S,
            ytdlp_max_sleep_interval_s: DEFAULT_YTDLP_MAX_SLEEP_INTERVAL_S,
            max_consecutive_errors_before_shelve: DEFAULT_MAX_CONSECUTIVE_ERRORS_BEFORE_SHELVE,
            notify_on_shelve: DEFAULT_NOTIFY_ON_SHELVE,
            idle_tick: Duration::from_secs(DEFAULT_IDLE_TICK_S),
        }
    }
}

/// Raw `app_config` values for the backfill tunables, returned alongside
/// the effective config so the diagnostics UI can warn the operator
/// when a stored value was clamped out by range validation.
#[derive(Debug, Clone, Default)]
pub struct BackfillConfigRaw {
    pub enabled: Option<String>,
    pub min_gap_between_channels_s: Option<String>,
    pub re_backfill_interval_s: Option<String>,
    pub subprocess_timeout_s: Option<String>,
    pub ytdlp_sleep_requests_s: Option<String>,
    pub ytdlp_sleep_interval_s: Option<String>,
    pub ytdlp_max_sleep_interval_s: Option<String>,
    pub max_consecutive_errors_before_shelve: Option<String>,
    pub notify_on_shelve: Option<String>,
    pub idle_tick_s: Option<String>,
}

impl BackfillConfig {
    /// Load and validate the effective config. Bad/missing values
    /// silently fall back to defaults.
    pub async fn load(pool: &SqlitePool) -> Self {
        let (cfg, _) = Self::load_with_raw(pool).await;
        cfg
    }

    /// Load the effective config and the raw values from `app_config`
    /// in a single query.
    pub async fn load_with_raw(pool: &SqlitePool) -> (Self, BackfillConfigRaw) {
        let mut raw = BackfillConfigRaw::default();
        let rows = sqlx::query_as::<_, (String, String)>(
            "SELECT key, value FROM app_config \
             WHERE key IN (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(KEY_ENABLED)
        .bind(KEY_MIN_GAP_BETWEEN_CHANNELS_S)
        .bind(KEY_RE_BACKFILL_INTERVAL_S)
        .bind(KEY_SUBPROCESS_TIMEOUT_S)
        .bind(KEY_YTDLP_SLEEP_REQUESTS_S)
        .bind(KEY_YTDLP_SLEEP_INTERVAL_S)
        .bind(KEY_YTDLP_MAX_SLEEP_INTERVAL_S)
        .bind(KEY_MAX_CONSECUTIVE_ERRORS_BEFORE_SHELVE)
        .bind(KEY_NOTIFY_ON_SHELVE)
        .bind(KEY_IDLE_TICK_S)
        .fetch_all(pool)
        .await
        .unwrap_or_default();
        for (k, v) in rows {
            match k.as_str() {
                KEY_ENABLED => raw.enabled = Some(v),
                KEY_MIN_GAP_BETWEEN_CHANNELS_S => raw.min_gap_between_channels_s = Some(v),
                KEY_RE_BACKFILL_INTERVAL_S => raw.re_backfill_interval_s = Some(v),
                KEY_SUBPROCESS_TIMEOUT_S => raw.subprocess_timeout_s = Some(v),
                KEY_YTDLP_SLEEP_REQUESTS_S => raw.ytdlp_sleep_requests_s = Some(v),
                KEY_YTDLP_SLEEP_INTERVAL_S => raw.ytdlp_sleep_interval_s = Some(v),
                KEY_YTDLP_MAX_SLEEP_INTERVAL_S => raw.ytdlp_max_sleep_interval_s = Some(v),
                KEY_MAX_CONSECUTIVE_ERRORS_BEFORE_SHELVE => {
                    raw.max_consecutive_errors_before_shelve = Some(v)
                }
                KEY_NOTIFY_ON_SHELVE => raw.notify_on_shelve = Some(v),
                KEY_IDLE_TICK_S => raw.idle_tick_s = Some(v),
                _ => {}
            }
        }

        let mut cfg = BackfillConfig::default();
        if let Some(v) = raw.enabled.as_deref() {
            match v {
                "true" => cfg.enabled = true,
                "false" => cfg.enabled = false,
                _ => {}
            }
        }
        if let Some(v) = raw
            .min_gap_between_channels_s
            .as_deref()
            .and_then(|s| s.parse::<u64>().ok())
        {
            if RANGE_MIN_GAP_BETWEEN_CHANNELS_S.contains(&v) {
                cfg.min_gap_between_channels = Duration::from_secs(v);
            }
        }
        if let Some(v) = raw
            .re_backfill_interval_s
            .as_deref()
            .and_then(|s| s.parse::<u64>().ok())
        {
            if RANGE_RE_BACKFILL_INTERVAL_S.contains(&v) {
                cfg.re_backfill_interval = Duration::from_secs(v);
            }
        }
        if let Some(v) = raw
            .subprocess_timeout_s
            .as_deref()
            .and_then(|s| s.parse::<u64>().ok())
        {
            if RANGE_SUBPROCESS_TIMEOUT_S.contains(&v) {
                cfg.subprocess_timeout = Duration::from_secs(v);
            }
        }
        if let Some(v) = raw
            .ytdlp_sleep_requests_s
            .as_deref()
            .and_then(|s| s.parse::<u32>().ok())
        {
            if RANGE_YTDLP_SLEEP_S.contains(&v) {
                cfg.ytdlp_sleep_requests_s = v;
            }
        }
        if let Some(v) = raw
            .ytdlp_sleep_interval_s
            .as_deref()
            .and_then(|s| s.parse::<u32>().ok())
        {
            if RANGE_YTDLP_SLEEP_S.contains(&v) {
                cfg.ytdlp_sleep_interval_s = v;
            }
        }
        if let Some(v) = raw
            .ytdlp_max_sleep_interval_s
            .as_deref()
            .and_then(|s| s.parse::<u32>().ok())
        {
            if RANGE_YTDLP_MAX_SLEEP_S.contains(&v) {
                cfg.ytdlp_max_sleep_interval_s = v;
            }
        }
        if let Some(v) = raw
            .max_consecutive_errors_before_shelve
            .as_deref()
            .and_then(|s| s.parse::<i64>().ok())
        {
            if RANGE_MAX_CONSECUTIVE_ERRORS.contains(&v) {
                cfg.max_consecutive_errors_before_shelve = v;
            }
        }
        if let Some(v) = raw.notify_on_shelve.as_deref() {
            match v {
                "true" => cfg.notify_on_shelve = true,
                "false" => cfg.notify_on_shelve = false,
                _ => {}
            }
        }
        if let Some(v) = raw
            .idle_tick_s
            .as_deref()
            .and_then(|s| s.parse::<u64>().ok())
        {
            if RANGE_IDLE_TICK_S.contains(&v) {
                cfg.idle_tick = Duration::from_secs(v);
            }
        }
        (cfg, raw)
    }
}

/// Public entry point: spawn the backfiller onto the current runtime.
/// Hands back a `JoinHandle` purely for testability; production code
/// just discards it.
pub fn spawn(pool: SqlitePool, cfg: Config) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        run(pool, cfg).await;
    })
}

/// Background backfill loop. Exposed (and hidden from rustdoc) only
/// so integration tests can spawn it directly. Production callers
/// should use [`spawn`].
#[doc(hidden)]
pub async fn run(pool: SqlitePool, cfg: Config) {
    info!("channel backfill loop starting");
    loop {
        let bcfg = BackfillConfig::load(&pool).await;
        if !bcfg.enabled {
            debug!("channel backfill disabled; sleeping");
            tokio::time::sleep(bcfg.idle_tick).await;
            continue;
        }

        let now = unix_now();
        let claimed = match claim_one_due(&pool, now).await {
            Ok(c) => c,
            Err(err) => {
                warn!(%err, "channel backfill: claim_one_due failed");
                tokio::time::sleep(bcfg.idle_tick).await;
                continue;
            }
        };

        let Some(channel_id) = claimed else {
            tokio::time::sleep(bcfg.idle_tick).await;
            continue;
        };

        let started_at = unix_now();
        if let Err(err) = run_backfill_for(&pool, &cfg, &channel_id, started_at, &bcfg).await {
            warn!(channel_id = %channel_id, %err, "channel backfill: run_backfill_for failed");
        }

        // Serialised inter-channel gap with jitter. Even on failure
        // we observe the gap before the next claim, so a misbehaving
        // YouTube can't be retry-stormed.
        let gap = jittered_interval(bcfg.min_gap_between_channels, 0.15);
        tokio::time::sleep(Duration::from_secs(gap.max(1) as u64)).await;
    }
}

/// Atomically claim the next due channel: select the row with the
/// lowest `backfill_next_at` that is `pending` and ≤ now, set its
/// status to `running` and push `backfill_lease_expires_at` into the
/// future. Returns `None` when no row is due.
async fn claim_one_due(pool: &SqlitePool, now: i64) -> AppResult<Option<String>> {
    let lease_until = now.saturating_add(LEASE_SECS);
    // Two-step in a transaction to keep the claim atomic against
    // concurrent calls (e.g. the daily feed_gc reconcile racing the
    // loop). Single-concurrency by design means this is mostly
    // belt-and-braces, but cheap.
    let mut tx = pool.begin().await?;
    let row: Option<(String,)> = sqlx::query_as(
        "SELECT channel_id FROM channel_sync_state \
          WHERE backfill_status = 'pending' AND backfill_next_at <= ? \
          ORDER BY backfill_next_at ASC \
          LIMIT 1",
    )
    .bind(now)
    .fetch_optional(&mut *tx)
    .await?;
    let Some((channel_id,)) = row else {
        tx.commit().await?;
        return Ok(None);
    };
    sqlx::query(
        "UPDATE channel_sync_state SET \
             backfill_status              = 'running', \
             backfill_last_started_at     = ?, \
             backfill_last_attempted_at   = ?, \
             backfill_lease_expires_at    = ? \
         WHERE channel_id = ?",
    )
    .bind(now)
    .bind(now)
    .bind(lease_until)
    .bind(&channel_id)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(Some(channel_id))
}

/// Run a single channel's backfill: drive yt-dlp through the channel's
/// uploads tab, upsert observed videos into `channel_videos`,
/// reconcile tombstones for rows that pre-date this run and weren't
/// observed, then transition the channel back to `pending` with the
/// next due time set.
async fn run_backfill_for(
    pool: &SqlitePool,
    cfg: &Config,
    channel_id: &str,
    started_at: i64,
    bcfg: &BackfillConfig,
) -> AppResult<()> {
    let tunables = FlatPlaylistTunables {
        timeout: bcfg.subprocess_timeout,
        sleep_requests_s: bcfg.ytdlp_sleep_requests_s,
        sleep_interval_s: bcfg.ytdlp_sleep_interval_s,
        max_sleep_interval_s: bcfg.ytdlp_max_sleep_interval_s,
    };

    let outcome = ytdlp::flat_playlist_channel(cfg, channel_id, &tunables).await;

    let result = match outcome {
        Ok(r) => r,
        Err(err) => {
            let err_msg = err.to_string();
            return record_failure(pool, channel_id, &err_msg, bcfg).await;
        }
    };

    // Persist observed entries + reconcile tombstones in a single tx.
    let mut tx = pool.begin().await?;
    let mut observed_ids: Vec<String> = Vec::with_capacity(result.entries.len());
    let mut new_count: i64 = 0;
    for entry in &result.entries {
        let (published_at, published_raw) = parse_upload_date(entry);
        let title = entry.title.clone().unwrap_or_else(|| entry.video_id.clone());
        let thumbnail_url = format!("https://i.ytimg.com/vi/{}/hqdefault.jpg", entry.video_id);
        let duration_s: Option<i64> = entry.duration.map(|d| d.round() as i64);

        // Detect insert vs update for stats purposes. We just need
        // to know whether a row exists; the per-row `is_deleted` /
        // `source` columns are updated by the upsert below regardless.
        let exists: Option<(i64,)> = sqlx::query_as(
            "SELECT 1 FROM channel_videos \
              WHERE channel_id = ? AND video_id = ?",
        )
        .bind(channel_id)
        .bind(&entry.video_id)
        .fetch_optional(&mut *tx)
        .await?;
        if exists.is_none() {
            new_count += 1;
        }

        sqlx::query(
            "INSERT INTO channel_videos \
                 (channel_id, video_id, title, channel_title, published_at, published_raw, \
                  duration_s, view_count, thumbnail_url, \
                  first_seen_at, last_seen_at, source, is_deleted) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?10, 'backfill', 0) \
             ON CONFLICT(channel_id, video_id) DO UPDATE SET \
                 title          = excluded.title, \
                 channel_title  = COALESCE(excluded.channel_title, channel_videos.channel_title), \
                 published_at   = COALESCE(channel_videos.published_at, excluded.published_at), \
                 published_raw  = COALESCE(channel_videos.published_raw, excluded.published_raw), \
                 duration_s     = COALESCE(excluded.duration_s, channel_videos.duration_s), \
                 view_count     = COALESCE(excluded.view_count, channel_videos.view_count), \
                 thumbnail_url  = COALESCE(channel_videos.thumbnail_url, excluded.thumbnail_url), \
                 last_seen_at   = excluded.last_seen_at, \
                 source         = 'backfill', \
                 is_deleted     = 0",
        )
        .bind(channel_id)
        .bind(&entry.video_id)
        .bind(&title)
        .bind(&entry.channel)
        .bind(published_at)
        .bind(&published_raw)
        .bind(duration_s)
        .bind(entry.view_count)
        .bind(&thumbnail_url)
        .bind(started_at)
        .execute(&mut *tx)
        .await?;

        observed_ids.push(entry.video_id.clone());
    }

    // Reconcile tombstones. Only tombstone rows that:
    //   - belong to this channel,
    //   - aren't already tombstoned,
    //   - existed BEFORE this backfill started (first_seen_at < started_at),
    //   - and weren't observed in this run.
    // The `first_seen_at < started_at` guard is the critical safety
    // property — without it, RSS upserts that land mid-backfill would
    // get tombstoned at commit.
    //
    // We build the JSON array client-side rather than using `json_each`
    // on a bound parameter so the SQL stays simple and portable.
    let observed_json = serde_json::to_string(&observed_ids).unwrap_or_else(|_| "[]".to_string());
    let removed_result = sqlx::query(
        "UPDATE channel_videos \
            SET is_deleted = 1 \
          WHERE channel_id = ?1 \
            AND is_deleted = 0 \
            AND first_seen_at < ?2 \
            AND video_id NOT IN (SELECT value FROM json_each(?3))",
    )
    .bind(channel_id)
    .bind(started_at)
    .bind(&observed_json)
    .execute(&mut *tx)
    .await?;
    let removed_count = removed_result.rows_affected() as i64;

    tx.commit().await?;

    // Schedule the next re-backfill with ±5% jitter so a wave of
    // simultaneously-completed channels doesn't synchronise on the
    // 30-day boundary.
    let next_at = unix_now() + jittered_interval(bcfg.re_backfill_interval, 0.05);
    let observed_total = result.entries.len() as i64;
    sqlx::query(
        "UPDATE channel_sync_state SET \
             backfill_status                  = 'complete', \
             backfill_last_completed_at       = ?, \
             backfill_last_error              = NULL, \
             backfill_consecutive_errors      = 0, \
             backfill_next_at                 = ?, \
             backfill_lease_expires_at        = NULL, \
             backfill_videos_observed_total   = ?, \
             backfill_videos_new_last_run     = ?, \
             backfill_videos_removed_last_run = ? \
         WHERE channel_id = ?",
    )
    .bind(unix_now())
    .bind(next_at)
    .bind(observed_total)
    .bind(new_count)
    .bind(removed_count)
    .bind(channel_id)
    .execute(pool)
    .await?;

    // After a successful run, immediately flip status back to 'pending'
    // so the next iteration of the loop *could* re-enter it (no row is
    // due though, because backfill_next_at is in the future). Keeping
    // 'complete' as a terminal state would require a separate sweep
    // to revive due rows, which is overhead with no benefit.
    sqlx::query(
        "UPDATE channel_sync_state SET backfill_status = 'pending' \
         WHERE channel_id = ? AND backfill_status = 'complete'",
    )
    .bind(channel_id)
    .execute(pool)
    .await?;

    info!(
        channel_id = %channel_id,
        observed = observed_total,
        new = new_count,
        removed = removed_count,
        "channel backfill completed"
    );
    Ok(())
}

/// Failure-classification + state-transition for one failed backfill.
async fn record_failure(
    pool: &SqlitePool,
    channel_id: &str,
    err_msg: &str,
    bcfg: &BackfillConfig,
) -> AppResult<()> {
    // Channel-not-found: single-strike shelve. No notification — most
    // likely a renamed/deleted channel that was on someone's allowlist.
    if is_not_found_error(err_msg) {
        sqlx::query(
            "UPDATE channel_sync_state SET \
                 backfill_status            = 'shelved', \
                 backfill_last_error        = ?, \
                 backfill_lease_expires_at  = NULL, \
                 backfill_next_at           = ? \
             WHERE channel_id = ?",
        )
        .bind("channel_not_found")
        .bind(unix_now() + (365 * 24 * 60 * 60))
        .bind(channel_id)
        .execute(pool)
        .await?;
        warn!(channel_id = %channel_id, "channel backfill shelved: not found");
        return Ok(());
    }

    let (consecutive_errors,): (i64,) = sqlx::query_as(
        "SELECT backfill_consecutive_errors FROM channel_sync_state WHERE channel_id = ?",
    )
    .bind(channel_id)
    .fetch_one(pool)
    .await?;
    let attempt = consecutive_errors + 1;
    let limit = bcfg.max_consecutive_errors_before_shelve;

    if attempt >= limit {
        // Shelve and (optionally) notify.
        let channel_title: Option<String> = sqlx::query_scalar(
            "SELECT channel_title FROM channel_sync_state WHERE channel_id = ?",
        )
        .bind(channel_id)
        .fetch_optional(pool)
        .await?
        .flatten();
        sqlx::query(
            "UPDATE channel_sync_state SET \
                 backfill_status              = 'shelved', \
                 backfill_last_error          = ?, \
                 backfill_consecutive_errors  = ?, \
                 backfill_lease_expires_at    = NULL \
             WHERE channel_id = ?",
        )
        .bind(err_msg)
        .bind(attempt)
        .bind(channel_id)
        .execute(pool)
        .await?;
        if bcfg.notify_on_shelve {
            let _ = crate::services::notifications::dispatch_channel_backfill_error_deduped(
                pool,
                channel_id,
                channel_title.as_deref(),
                err_msg,
            )
            .await;
        }
        warn!(
            channel_id = %channel_id,
            attempt,
            limit,
            err = %err_msg,
            "channel backfill shelved after consecutive failures"
        );
    } else {
        // Backoff and retry later.
        let backoff = backoff_for_attempt(attempt, bcfg.min_gap_between_channels);
        let next_at = unix_now() + backoff;
        sqlx::query(
            "UPDATE channel_sync_state SET \
                 backfill_status              = 'pending', \
                 backfill_last_error          = ?, \
                 backfill_consecutive_errors  = ?, \
                 backfill_next_at             = ?, \
                 backfill_lease_expires_at    = NULL \
             WHERE channel_id = ?",
        )
        .bind(err_msg)
        .bind(attempt)
        .bind(next_at)
        .bind(channel_id)
        .execute(pool)
        .await?;
        warn!(
            channel_id = %channel_id,
            attempt,
            backoff,
            err = %err_msg,
            "channel backfill failed; backing off"
        );
    }
    Ok(())
}

/// Best-effort classification: does the error look like a YouTube
/// "channel does not exist" signal? Cheap pattern match against the
/// stderr tail produced by yt-dlp; if anything looks like a 404 / not
/// found / unavailable, treat it as terminal.
fn is_not_found_error(err: &str) -> bool {
    let lower = err.to_ascii_lowercase();
    lower.contains("does not exist")
        || lower.contains("not found")
        || lower.contains("404")
        || lower.contains("this channel was terminated")
        || lower.contains("unavailable")
}

/// Best-effort classification: does the error look like a YouTube
/// bot-check signal? Used by callers/tests to verify the loop handles
/// these correctly (currently all classified failures go through the
/// generic retry/shelve path; this is reserved for future routing).
#[allow(dead_code)]
pub(crate) fn is_bot_check_error(err: &str) -> bool {
    let lower = err.to_ascii_lowercase();
    lower.contains("sign in")
        || lower.contains("sign-in")
        || lower.contains("consent")
        || lower.contains("captcha")
        || lower.contains("403")
        || lower.contains("429")
        || lower.contains("rate")
        || lower.contains("forbidden")
}

/// Parse yt-dlp's `upload_date` field (typically `YYYYMMDD`) into a
/// unix-seconds timestamp. Returns `(published_at, published_raw)`.
fn parse_upload_date(entry: &FlatPlaylistEntry) -> (Option<i64>, Option<String>) {
    let raw = entry.upload_date.clone();
    let parsed = raw.as_deref().and_then(|s| {
        let s = s.trim();
        if s.len() != 8 {
            return None;
        }
        let year: i32 = s.get(0..4)?.parse().ok()?;
        let month: u32 = s.get(4..6)?.parse().ok()?;
        let day: u32 = s.get(6..8)?.parse().ok()?;
        let date = chrono::NaiveDate::from_ymd_opt(year, month, day)?;
        let dt = date.and_hms_opt(0, 0, 0)?;
        Some(dt.and_utc().timestamp())
    });
    (parsed, raw)
}

// ---------------------------------------------------------------------------
// Public API for routes / tests / cron
// ---------------------------------------------------------------------------

/// Seed `channel_sync_state` rows for every channel in `allowlisted_channels`,
/// then GC any rows / `channel_videos` for channels no longer allowlisted
/// by any child. Idempotent — safe to call at startup and from the
/// `feed_gc` cron.
pub async fn reconcile_with_allowlist(pool: &SqlitePool) -> AppResult<()> {
    let mut tx = pool.begin().await?;

    // 1. Seed rows for any newly-allowlisted channel. Existing rows
    //    are untouched (ON CONFLICT DO NOTHING). The trailing
    //    `WHERE true` disambiguates the UPSERT's `ON CONFLICT` from
    //    a potential JOIN `ON` clause — see
    //    https://www.sqlite.org/lang_upsert.html ("Parsing Ambiguity").
    sqlx::query(
        "INSERT INTO channel_sync_state \
             (channel_id, backfill_status, backfill_next_at, rss_next_poll_at) \
         SELECT DISTINCT channel_id, 'pending', 0, 0 \
           FROM allowlisted_channels WHERE true \
         ON CONFLICT(channel_id) DO NOTHING",
    )
    .execute(&mut *tx)
    .await?;

    // 2. GC channels no longer allowlisted. channel_videos first so
    //    we don't strand orphan video rows.
    sqlx::query(
        "DELETE FROM channel_videos \
          WHERE channel_id NOT IN (SELECT channel_id FROM allowlisted_channels)",
    )
    .execute(&mut *tx)
    .await?;
    sqlx::query(
        "DELETE FROM channel_sync_state \
          WHERE channel_id NOT IN (SELECT channel_id FROM allowlisted_channels)",
    )
    .execute(&mut *tx)
    .await?;

    tx.commit().await?;
    Ok(())
}

/// Set `backfill_next_at = 0` for a single channel, bumping it to the
/// front of the queue. Used by the admin "Run now" route. No-op if the
/// channel isn't in `channel_sync_state` or is currently `running`.
pub async fn enqueue_run_now(pool: &SqlitePool, channel_id: &str) -> AppResult<u64> {
    let res = sqlx::query(
        "UPDATE channel_sync_state SET backfill_next_at = 0 \
          WHERE channel_id = ? \
            AND backfill_status IN ('pending', 'failed', 'complete')",
    )
    .bind(channel_id)
    .execute(pool)
    .await?;
    Ok(res.rows_affected())
}

/// Clear a shelved channel's error state and move it back to pending.
/// Used by the admin "Unshelve" route after the operator has fixed
/// whatever caused the channel to be shelved.
pub async fn unshelve(pool: &SqlitePool, channel_id: &str) -> AppResult<u64> {
    let res = sqlx::query(
        "UPDATE channel_sync_state SET \
             backfill_status              = 'pending', \
             backfill_last_error          = NULL, \
             backfill_consecutive_errors  = 0, \
             backfill_next_at             = 0, \
             backfill_lease_expires_at    = NULL \
         WHERE channel_id = ? AND backfill_status = 'shelved'",
    )
    .bind(channel_id)
    .execute(pool)
    .await?;
    Ok(res.rows_affected())
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// `interval ± frac` jittered, returned in seconds. `frac` is the
/// half-width of the jitter band (e.g. 0.15 = ±15%, 0.05 = ±5%).
pub fn jittered_interval(interval: Duration, frac: f64) -> i64 {
    let secs = interval.as_secs() as f64;
    let jitter_frac: f64 = rand::rng().random_range(-frac..frac);
    (secs + secs * jitter_frac).round() as i64
}

/// Exponential backoff with jitter, capped at [`MAX_BACKOFF`]. Mirrors
/// the freshness refresher's helper. `attempt` is the post-failure
/// error count (1 for the first failure).
pub fn backoff_for_attempt(attempt: i64, interval: Duration) -> i64 {
    let base = interval.as_secs() as f64;
    let exp = attempt.clamp(1, 16) as u32;
    let target = base * (2u64.pow(exp) as f64);
    let capped = target.min(MAX_BACKOFF.as_secs() as f64);
    let jitter_frac: f64 = rand::rng().random_range(-0.15..0.15);
    (capped + capped * jitter_frac).round() as i64
}

pub fn unix_now() -> i64 {
    chrono::Utc::now().timestamp()
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
    use std::str::FromStr;

    async fn setup_db() -> SqlitePool {
        let opts = SqliteConnectOptions::from_str("sqlite::memory:")
            .unwrap()
            .foreign_keys(true);
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(opts)
            .await
            .unwrap();
        sqlx::migrate!("./migrations").run(&pool).await.unwrap();
        pool
    }

    async fn allow_channel(pool: &SqlitePool, channel_id: &str) {
        // Need a child account to satisfy the FK on allowlisted_channels.
        let cnt: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM accounts")
            .fetch_one(pool)
            .await
            .unwrap();
        let child_id = if cnt == 0 {
            sqlx::query(
                "INSERT INTO accounts (display_name, account_type, pin_hash, created_at, updated_at) \
                 VALUES ('kid', 'child', 'x', unixepoch(), unixepoch())",
            )
            .execute(pool)
            .await
            .unwrap();
            sqlx::query_scalar::<_, i64>("SELECT last_insert_rowid()")
                .fetch_one(pool)
                .await
                .unwrap()
        } else {
            sqlx::query_scalar::<_, i64>("SELECT id FROM accounts LIMIT 1")
                .fetch_one(pool)
                .await
                .unwrap()
        };
        sqlx::query(
            "INSERT INTO allowlisted_channels \
                (child_account_id, channel_id, channel_title, added_by) \
             VALUES (?, ?, 'X', ?)",
        )
        .bind(child_id)
        .bind(channel_id)
        .bind(child_id)
        .execute(pool)
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn reconcile_seeds_and_gcs() {
        let pool = setup_db().await;
        allow_channel(&pool, "UC1").await;
        allow_channel(&pool, "UC2").await;

        reconcile_with_allowlist(&pool).await.unwrap();
        let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM channel_sync_state")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(n, 2);

        // Remove UC2 from allowlist; reconcile drops the row.
        sqlx::query("DELETE FROM allowlisted_channels WHERE channel_id = 'UC2'")
            .execute(&pool)
            .await
            .unwrap();
        // Also seed a video for UC2 so we can prove the cascade.
        sqlx::query(
            "INSERT INTO channel_videos \
                 (channel_id, video_id, title, first_seen_at, last_seen_at, source) \
             VALUES ('UC2', 'vGone', 'X', 1, 1, 'rss')",
        )
        .execute(&pool)
        .await
        .unwrap();

        reconcile_with_allowlist(&pool).await.unwrap();
        let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM channel_sync_state")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(n, 1);
        let vn: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM channel_videos WHERE channel_id = 'UC2'")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(vn, 0, "channel_videos cascade-deleted with channel_sync_state");
    }

    #[tokio::test]
    async fn enqueue_run_now_bumps_pending_channel() {
        let pool = setup_db().await;
        allow_channel(&pool, "UC1").await;
        reconcile_with_allowlist(&pool).await.unwrap();
        // Push the channel's next_at far into the future to simulate a
        // freshly-backfilled channel.
        sqlx::query(
            "UPDATE channel_sync_state SET backfill_next_at = 9_999_999, \
                                            backfill_last_completed_at = 1 \
              WHERE channel_id = 'UC1'",
        )
        .execute(&pool)
        .await
        .unwrap();

        let updated = enqueue_run_now(&pool, "UC1").await.unwrap();
        assert_eq!(updated, 1);
        let next: i64 = sqlx::query_scalar(
            "SELECT backfill_next_at FROM channel_sync_state WHERE channel_id = 'UC1'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(next, 0, "run-now must set backfill_next_at = 0");
    }

    #[tokio::test]
    async fn enqueue_run_now_skips_running_channel() {
        let pool = setup_db().await;
        allow_channel(&pool, "UC1").await;
        reconcile_with_allowlist(&pool).await.unwrap();
        sqlx::query(
            "UPDATE channel_sync_state SET backfill_status = 'running', backfill_next_at = 100 \
              WHERE channel_id = 'UC1'",
        )
        .execute(&pool)
        .await
        .unwrap();

        let updated = enqueue_run_now(&pool, "UC1").await.unwrap();
        assert_eq!(updated, 0);
        let next: i64 = sqlx::query_scalar(
            "SELECT backfill_next_at FROM channel_sync_state WHERE channel_id = 'UC1'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(next, 100, "running channel must not be re-enqueued");
    }

    #[tokio::test]
    async fn unshelve_clears_shelved_state() {
        let pool = setup_db().await;
        allow_channel(&pool, "UC1").await;
        reconcile_with_allowlist(&pool).await.unwrap();
        sqlx::query(
            "UPDATE channel_sync_state SET \
                 backfill_status              = 'shelved', \
                 backfill_last_error          = 'boom', \
                 backfill_consecutive_errors  = 7, \
                 backfill_next_at             = 9_999_999 \
              WHERE channel_id = 'UC1'",
        )
        .execute(&pool)
        .await
        .unwrap();

        let updated = unshelve(&pool, "UC1").await.unwrap();
        assert_eq!(updated, 1);

        let (status, err, errs, next): (String, Option<String>, i64, i64) = sqlx::query_as(
            "SELECT backfill_status, backfill_last_error, backfill_consecutive_errors, \
                    backfill_next_at \
               FROM channel_sync_state WHERE channel_id = 'UC1'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(status, "pending");
        assert_eq!(err, None);
        assert_eq!(errs, 0);
        assert_eq!(next, 0);
    }

    #[tokio::test]
    async fn claim_one_due_returns_only_due_pending_rows() {
        let pool = setup_db().await;
        allow_channel(&pool, "UC1").await;
        allow_channel(&pool, "UC2").await;
        reconcile_with_allowlist(&pool).await.unwrap();
        // UC2's backfill_next_at is in the future.
        sqlx::query(
            "UPDATE channel_sync_state SET backfill_next_at = 9_999_999 WHERE channel_id = 'UC2'",
        )
        .execute(&pool)
        .await
        .unwrap();

        let claimed = claim_one_due(&pool, 1_000).await.unwrap();
        assert_eq!(claimed.as_deref(), Some("UC1"));
        // Status is now 'running'; a second claim returns the other
        // due row only if it were eligible — UC2 isn't due yet.
        let claimed = claim_one_due(&pool, 1_000).await.unwrap();
        assert_eq!(claimed, None);
    }

    #[test]
    fn parse_upload_date_handles_yyyymmdd() {
        let entry = FlatPlaylistEntry {
            video_id: "x".into(),
            title: None,
            upload_date: Some("20240115".to_string()),
            duration: None,
            view_count: None,
            channel: None,
            channel_id: None,
        };
        let (at, raw) = parse_upload_date(&entry);
        assert_eq!(raw.as_deref(), Some("20240115"));
        assert!(at.is_some());
        // 2024-01-15 UTC midnight = 1705276800
        assert_eq!(at, Some(1_705_276_800));
    }

    #[test]
    fn parse_upload_date_rejects_garbage() {
        let entry = FlatPlaylistEntry {
            video_id: "x".into(),
            title: None,
            upload_date: Some("invalid".to_string()),
            duration: None,
            view_count: None,
            channel: None,
            channel_id: None,
        };
        let (at, _) = parse_upload_date(&entry);
        assert_eq!(at, None);
    }

    #[test]
    fn jitter_stays_within_band() {
        for _ in 0..200 {
            let v = jittered_interval(Duration::from_secs(3600), 0.15);
            assert!(v >= (3600.0 * 0.85 - 1.0) as i64 && v <= (3600.0 * 1.15 + 1.0) as i64);
        }
    }

    #[test]
    fn backoff_grows_then_caps() {
        let base = Duration::from_secs(3600);
        let one = backoff_for_attempt(1, base);
        let two = backoff_for_attempt(2, base);
        let huge = backoff_for_attempt(50, base);
        assert!(one < two);
        // Eventually we hit the 24h cap.
        let cap = MAX_BACKOFF.as_secs() as i64;
        assert!(huge <= (cap as f64 * 1.16) as i64);
        assert!(huge >= (cap as f64 * 0.84) as i64);
    }

    #[test]
    fn is_not_found_error_classifies_404_signals() {
        assert!(is_not_found_error("HTTP Error 404: Not Found"));
        assert!(is_not_found_error("This channel does not exist"));
        assert!(is_not_found_error("This channel was terminated"));
        assert!(!is_not_found_error("Sign in to confirm you're not a bot"));
        assert!(!is_not_found_error("rate-limit"));
    }

    #[test]
    fn is_bot_check_error_classifies_well_known_signals() {
        assert!(is_bot_check_error("Sign in to confirm you're not a bot"));
        assert!(is_bot_check_error("Got HTTP 429"));
        assert!(is_bot_check_error("consent.youtube.com redirect"));
        assert!(!is_bot_check_error("HTTP Error 404"));
    }
}
