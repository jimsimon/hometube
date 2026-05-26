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

use std::collections::HashSet;
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

/// Cooldown applied AFTER a sidecar fallback for the same channel
/// before the backfill loop will pick the row up. Avoids stacking two
/// anti-bot-sensitive operations (a sidecar `/channel-videos` call and
/// a yt-dlp `--flat-playlist` subprocess) on the same channel
/// back-to-back. Mirrors the "additional hour" defer from the plan's
/// anti-bot strategy item #7.
///
/// One hour matches the per-source sidecar fallback min interval for
/// active channels, so a channel that just took a sidecar fallback
/// won't be backfill-claimed before its next sidecar fallback would
/// even be eligible. Effectively serialises the two anti-bot paths
/// per channel.
pub const SIDECAR_FALLBACK_COOLDOWN_SECS: i64 = 60 * 60;

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
    let sidecar_cutoff = now.saturating_sub(SIDECAR_FALLBACK_COOLDOWN_SECS);
    // Two-step in a transaction to keep the claim atomic against
    // concurrent calls (e.g. the daily feed_gc reconcile racing the
    // loop). Single-concurrency by design means this is mostly
    // belt-and-braces, but cheap.
    //
    // Three conditions on the WHERE:
    //
    // 1. **Status gate**: the channel is either `'pending'` (the
    //    common case) OR `'running'` with an expired lease. The
    //    second arm recovers stranded rows: if a worker crashed
    //    between `claim_one_due` flipping the status to `'running'`
    //    and the eventual reset to `'pending'`/`'shelved'`, the
    //    `backfill_lease_expires_at` we wrote on claim will eventually
    //    fall below `now` and the row becomes reclaimable. Without
    //    this clause, a single crash mid-`apply_backfill_entries`
    //    would strand the channel forever.
    //
    // 2. **Schedule gate**: `backfill_next_at <= now` — only claim
    //    channels whose backoff window has elapsed.
    //
    // 3. **Sidecar cooldown**: defers a backfill by up to
    //    `SIDECAR_FALLBACK_COOLDOWN_SECS` after the freshness loop
    //    took a sidecar fallback for this channel. The two anti-bot
    //    paths (sidecar `/channel-videos` and yt-dlp `--flat-playlist`)
    //    end up talking to InnerTube for the same channel, and
    //    stacking them back-to-back is more likely to provoke YouTube
    //    than spacing them out by an hour.
    let mut tx = pool.begin().await?;
    let row: Option<(String,)> = sqlx::query_as(
        "SELECT channel_id FROM channel_sync_state \
          WHERE ( \
                backfill_status = 'pending' \
                OR (backfill_status = 'running' \
                    AND backfill_lease_expires_at IS NOT NULL \
                    AND backfill_lease_expires_at <= ?) \
              ) \
            AND backfill_next_at <= ? \
            AND (last_sidecar_fallback_at IS NULL \
                 OR last_sidecar_fallback_at <= ?) \
          ORDER BY backfill_next_at ASC \
          LIMIT 1",
    )
    .bind(now)
    .bind(now)
    .bind(sidecar_cutoff)
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

    let newly_observed =
        apply_backfill_entries(pool, channel_id, started_at, &result.entries, bcfg).await?;

    // Tail-call: enqueue a slow prefetch of the thumbnails for any
    // newly-observed videos. Best-effort — failures are logged but
    // don't fail the backfill. The prefetcher is rate-limited
    // internally (~1 image/sec) so even a 10k-video channel's first
    // pass settles within a couple of hours.
    if !newly_observed.is_empty() {
        let cache_dir = cfg.cache_dir.clone();
        let pool = pool.clone();
        tokio::spawn(async move {
            prefetch_thumbnails(&pool, &cache_dir, &newly_observed).await;
        });
    }
    Ok(())
}

/// User-Agent sent on the thumbnail prefetch HTTP requests. Mirrors a
/// modern desktop browser instead of the default `reqwest/x.y.z` so
/// thousands of requests to `i.ytimg.com` from one host don't carry
/// an obvious automation fingerprint. (`i.ytimg.com` is YouTube's
/// thumbnail CDN; ordinary browsers fetching `<img src>` would set a
/// User-Agent like this one.)
const PREFETCH_USER_AGENT: &str =
    "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) \
     Chrome/126.0.0.0 Safari/537.36";

/// Slow-rate background fetcher that populates the `thumbnail_cache`
/// for a batch of newly-observed videos. ~1 request/second; on
/// failure (network, 4xx) we just skip the video and continue —
/// `GET /api/proxy/thumbnail/:videoId` will fall back to the
/// on-demand fetch path.
///
/// Spawned via `tokio::spawn` from `run_backfill_for` without
/// JoinHandle tracking. On runtime shutdown the prefetch is silently
/// dropped mid-batch, which is acceptable because:
/// - The thumbnail cache is a pure performance optimisation; missing
///   entries fall back to the proxy on-demand path.
/// - Newly-observed videos that miss prefetching will be picked up
///   organically the next time a child renders the channel page.
/// - The next backfill of the same channel will re-observe the same
///   videos and re-enqueue prefetching from scratch.
async fn prefetch_thumbnails(pool: &SqlitePool, cache_dir: &str, video_ids: &[String]) {
    use std::time::Duration as StdDuration;
    let client = match reqwest::Client::builder()
        .timeout(StdDuration::from_secs(15))
        .user_agent(PREFETCH_USER_AGENT)
        .build()
    {
        Ok(c) => c,
        Err(err) => {
            warn!(%err, "prefetch_thumbnails: failed to build HTTP client");
            return;
        }
    };
    for video_id in video_ids {
        // Skip if a fresh entry already exists — could have been
        // populated by a concurrent proxy request between the
        // observation and the prefetch.
        let existing: Option<(String,)> =
            sqlx::query_as("SELECT file_path FROM thumbnail_cache WHERE video_id = ?")
                .bind(video_id)
                .fetch_optional(pool)
                .await
                .ok()
                .flatten();
        if existing.is_some() {
            continue;
        }

        // Try hqdefault first, then mqdefault as a fallback.
        let urls = [
            format!("https://i.ytimg.com/vi/{video_id}/hqdefault.jpg"),
            format!("https://i.ytimg.com/vi/{video_id}/mqdefault.jpg"),
        ];
        let mut stored = false;
        for url in &urls {
            match client.get(url).send().await {
                Ok(resp) if resp.status().is_success() => match resp.bytes().await {
                    Ok(bytes) => {
                        let _ = crate::services::thumbnail_store::put(
                            pool, cache_dir, video_id, &bytes,
                        )
                        .await;
                        stored = true;
                        break;
                    }
                    Err(err) => {
                        debug!(%video_id, %err, "prefetch: failed to read body");
                    }
                },
                Ok(resp) => {
                    debug!(%video_id, status = %resp.status(), "prefetch: non-success");
                }
                Err(err) => {
                    debug!(%video_id, %err, "prefetch: request failed");
                }
            }
        }
        if stored {
            // ~1 image/sec to keep the prefetch from spiking
            // i.ytimg.com. Only sleep when we actually fetched bytes
            // — a hard-failed video (network error, 404, etc.)
            // generates no upstream traffic, so there's nothing to
            // pace against.
            tokio::time::sleep(StdDuration::from_secs(1)).await;
        } else {
            debug!(%video_id, "prefetch: no thumbnail variant succeeded");
        }
    }
}

/// Apply a successful `flat_playlist_channel` result to the database:
/// upsert observed video stubs into `channel_videos`, reconcile
/// tombstones, and transition the channel's `backfill_status` back to
/// `pending` with the next due time set.
///
/// Extracted from [`run_backfill_for`] so unit tests can drive the DB
/// behavior with a canned `Vec<FlatPlaylistEntry>` without spawning
/// yt-dlp. The integration test in `tests/channel_backfill_ytdlp_shim.rs`
/// reaches in via [`apply_backfill_entries_for_testing`] (a pub wrapper
/// flagged `#[doc(hidden)]`).
#[doc(hidden)]
pub async fn apply_backfill_entries(
    pool: &SqlitePool,
    channel_id: &str,
    started_at: i64,
    entries: &[FlatPlaylistEntry],
    bcfg: &BackfillConfig,
) -> AppResult<Vec<String>> {
    // Persist observed entries + reconcile tombstones in a single tx.
    let mut tx = pool.begin().await?;

    // Pre-fetch every existing video_id for this channel into a HashSet
    // so the per-entry "is this row new?" check is O(1) instead of an
    // extra SELECT per row. For a 10k-video channel this collapses
    // ~10k DB round-trips into one — meaningful inside a single tx
    // where SQLite holds an exclusive write lock the whole time.
    let existing_ids: HashSet<String> =
        sqlx::query_scalar::<_, String>("SELECT video_id FROM channel_videos WHERE channel_id = ?")
            .bind(channel_id)
            .fetch_all(&mut *tx)
            .await?
            .into_iter()
            .collect();

    let mut observed_ids: Vec<String> = Vec::with_capacity(entries.len());
    // `new_video_ids` is returned to the caller so it can enqueue
    // thumbnail prefetching for genuinely-new uploads. Held outside
    // the tx so a commit failure doesn't promise prefetches that
    // never happened.
    let mut new_video_ids: Vec<String> = Vec::new();
    let mut new_count: i64 = 0;
    for entry in entries {
        let (published_at, published_raw) = parse_upload_date(entry);
        let title = entry
            .title
            .clone()
            .unwrap_or_else(|| entry.video_id.clone());
        let thumbnail_url = format!("https://i.ytimg.com/vi/{}/hqdefault.jpg", entry.video_id);
        let duration_s: Option<i64> = entry.duration.map(|d| d.round() as i64);

        // Detect insert vs update via the pre-fetched HashSet — no
        // extra round-trip per entry. The upsert below handles the
        // `is_deleted` / `source` reset regardless of insert-vs-update.
        if !existing_ids.contains(&entry.video_id) {
            new_count += 1;
            new_video_ids.push(entry.video_id.clone());
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
    // Implementation: stage the observed video_ids into a transaction-
    // local temporary table and `NOT IN (SELECT … FROM tmp)`. For a
    // 10k-video channel this avoids serialising ~150 KB of JSON
    // through a single SQLite parameter and lets the query planner
    // use a real index/hash join instead of `json_each`.
    sqlx::query("CREATE TEMP TABLE _backfill_observed (video_id TEXT PRIMARY KEY) WITHOUT ROWID")
        .execute(&mut *tx)
        .await?;
    for vid in &observed_ids {
        sqlx::query("INSERT OR IGNORE INTO _backfill_observed(video_id) VALUES (?)")
            .bind(vid)
            .execute(&mut *tx)
            .await?;
    }
    let removed_result = sqlx::query(
        "UPDATE channel_videos \
            SET is_deleted = 1 \
          WHERE channel_id = ?1 \
            AND is_deleted = 0 \
            AND first_seen_at < ?2 \
            AND video_id NOT IN (SELECT video_id FROM _backfill_observed)",
    )
    .bind(channel_id)
    .bind(started_at)
    .execute(&mut *tx)
    .await?;
    let removed_count = removed_result.rows_affected() as i64;
    // Drop the temp table so a second `apply_backfill_entries` call
    // on the same connection (rare but possible in test setups) can
    // re-create it cleanly.
    sqlx::query("DROP TABLE _backfill_observed")
        .execute(&mut *tx)
        .await?;

    tx.commit().await?;

    // Schedule the next re-backfill with ±5% jitter so a wave of
    // simultaneously-completed channels doesn't synchronise on the
    // 30-day boundary.
    //
    // We write `backfill_status = 'pending'` directly (skipping the
    // intermediate `'complete'`). The old two-step
    // ('running' → 'complete' → 'pending') was vulnerable to a
    // wedged-row scenario: if the process crashed between the two
    // UPDATEs, the row stayed `'complete'` and was never re-claimable
    // by the loop (which only matches `'pending'` and stale
    // `'running'`). `backfill_last_completed_at` carries the
    // completion-tracking semantic on its own — `'complete'` as a
    // status value is redundant. The status `CHECK` constraint still
    // permits it for forward compatibility / older row history.
    let next_at = unix_now() + jittered_interval(bcfg.re_backfill_interval, 0.05);
    let observed_total = entries.len() as i64;
    sqlx::query(
        "UPDATE channel_sync_state SET \
             backfill_status                  = 'pending', \
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

    info!(
        channel_id = %channel_id,
        observed = observed_total,
        new = new_count,
        removed = removed_count,
        "channel backfill completed"
    );
    Ok(new_video_ids)
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
        let channel_title: Option<String> =
            sqlx::query_scalar("SELECT channel_title FROM channel_sync_state WHERE channel_id = ?")
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
/// "channel permanently gone" signal?
///
/// Single-strike shelves the channel for 365 days, so this match must
/// be **conservative** — we'd rather miss a real not-found (and let
/// the normal exponential backoff drive retries) than mis-classify a
/// transient error as terminal and lock the channel out for a year.
///
/// Tightened patterns:
///
/// - `"this channel does not exist"` / `"channel does not exist"` —
///   exact yt-dlp message for a known-dead channel ID.
/// - `"this channel was terminated"` — exact yt-dlp message for a
///   policy-removed channel.
/// - `"http error 404"` / `" 404 "` / `"status 404"` — anchored on
///   the actual HTTP-error wording so we don't match an incidental
///   `"404"` substring inside a URL or stack trace.
///
/// Removed the bare `"not found"` (matches yt-dlp's
/// `"requested format not found"` for transient extractor failures)
/// and the bare `"unavailable"` (yt-dlp says
/// `"Sign in to confirm you're not a bot. Use --cookies-from-browser
///  …"` on bot walls, and many transient errors include the word
/// "unavailable" e.g. `"YouTube said: The service is unavailable"`).
fn is_not_found_error(err: &str) -> bool {
    let lower = err.to_ascii_lowercase();
    lower.contains("channel does not exist")
        || lower.contains("this channel was terminated")
        || lower.contains("http error 404")
        || lower.contains(" 404 ")
        || lower.contains("status 404")
        || lower.contains("returned 404")
}

/// Best-effort classification: does the error look like a YouTube
/// bot-check signal? Used by callers/tests to verify the loop handles
/// these correctly (currently all classified failures go through the
/// generic retry/shelve path; this is reserved for future routing).
///
/// Tightened to avoid the same loose-substring traps as
/// [`is_not_found_error`]: HTTP status codes match on
/// `"http error 403"` / `"http error 429"` rather than the bare
/// numbers (which appear in URLs and timestamps).
#[allow(dead_code)]
pub(crate) fn is_bot_check_error(err: &str) -> bool {
    let lower = err.to_ascii_lowercase();
    lower.contains("sign in to confirm")
        || lower.contains("consent.youtube.com")
        || lower.contains("captcha")
        || lower.contains("http error 403")
        || lower.contains("http error 429")
        || lower.contains("status 403")
        || lower.contains("status 429")
        || lower.contains("rate-limit")
        || lower.contains("rate limit")
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

    // 1. Seed rows for any newly-allowlisted channel, carrying over
    //    `channel_title` / `channel_thumbnail_url` from the allowlist
    //    so the admin diagnostics view (and the channel page header)
    //    have a name to display before the first sidecar run.
    //
    //    A channel can appear in `allowlisted_channels` once per child,
    //    so we `GROUP BY channel_id` and pick any non-null title /
    //    thumbnail via `MAX(...)` (titles for the same channel should
    //    match across children; if they ever disagree, picking
    //    deterministically is good enough for a diagnostics surface).
    //
    //    On conflict we `COALESCE` the existing value with the
    //    incoming one so we backfill rows whose metadata was NULL
    //    (e.g. seeded by an older version of this function before it
    //    propagated titles) without clobbering rows that already have
    //    a richer title from a sidecar refresh.
    // `MAX(...)` over sibling allowlist rows for the same channel
    // resolves the (rare) case where two children allowlisted the same
    // channel with subtly different display strings — picking the
    // lexicographically-largest under the default BINARY collation is
    // arbitrary but deterministic. Wrapping in `TRIM(...)` first
    // normalises trailing-whitespace divergences so a stray `"Algol "`
    // can't sort above `"Algol"` and end up as the canonical title.
    sqlx::query(
        "INSERT INTO channel_sync_state \
             (channel_id, channel_title, channel_thumbnail_url, \
              backfill_status, backfill_next_at, rss_next_poll_at) \
         SELECT channel_id, MAX(TRIM(channel_title)), MAX(TRIM(channel_thumbnail_url)), \
                'pending', 0, 0 \
           FROM allowlisted_channels \
          GROUP BY channel_id \
         ON CONFLICT(channel_id) DO UPDATE SET \
              channel_title         = COALESCE(channel_sync_state.channel_title, \
                                               excluded.channel_title), \
              channel_thumbnail_url = COALESCE(channel_sync_state.channel_thumbnail_url, \
                                               excluded.channel_thumbnail_url)",
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
        assert_eq!(
            vn, 0,
            "channel_videos cascade-deleted with channel_sync_state"
        );
    }

    /// `reconcile_with_allowlist` should carry `channel_title` over
    /// from `allowlisted_channels` so the admin diagnostics view can
    /// render channel names, and should backfill the title on rows
    /// that an older version of the function left NULL.
    #[tokio::test]
    async fn reconcile_propagates_channel_title() {
        let pool = setup_db().await;
        allow_channel(&pool, "UC1").await;

        // Pre-create a `channel_sync_state` row the way the older
        // (title-less) seed path used to: NULL title, NULL thumbnail.
        // This simulates a long-lived row in a real deployment.
        sqlx::query(
            "INSERT INTO channel_sync_state (channel_id, backfill_status, \
                                             backfill_next_at, rss_next_poll_at) \
             VALUES ('UC1', 'pending', 0, 0)",
        )
        .execute(&pool)
        .await
        .unwrap();

        reconcile_with_allowlist(&pool).await.unwrap();

        let title: Option<String> = sqlx::query_scalar(
            "SELECT channel_title FROM channel_sync_state WHERE channel_id = 'UC1'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(
            title.as_deref(),
            Some("X"),
            "reconcile should have backfilled the NULL title from allowlisted_channels"
        );

        // Seeding a brand-new channel should also pick up the title in
        // the same call.
        allow_channel(&pool, "UC2").await;
        reconcile_with_allowlist(&pool).await.unwrap();
        let title2: Option<String> = sqlx::query_scalar(
            "SELECT channel_title FROM channel_sync_state WHERE channel_id = 'UC2'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(title2.as_deref(), Some("X"));

        // An existing non-NULL title (e.g. set by a sidecar refresh)
        // must not be clobbered by reconcile.
        sqlx::query(
            "UPDATE channel_sync_state SET channel_title = 'Sidecar Title' \
              WHERE channel_id = 'UC1'",
        )
        .execute(&pool)
        .await
        .unwrap();
        reconcile_with_allowlist(&pool).await.unwrap();
        let title3: Option<String> = sqlx::query_scalar(
            "SELECT channel_title FROM channel_sync_state WHERE channel_id = 'UC1'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(
            title3.as_deref(),
            Some("Sidecar Title"),
            "reconcile must not clobber a richer title with the allowlist title"
        );
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

    // -----------------------------------------------------------------
    // apply_backfill_entries — the DB half of run_backfill_for, extracted
    // so tests don't need to spawn yt-dlp. The yt-dlp invocation itself
    // is mocked at the subprocess boundary (covered by the integration
    // test harness against a shim binary on PATH).
    // -----------------------------------------------------------------

    fn entry(video_id: &str, title: &str) -> FlatPlaylistEntry {
        FlatPlaylistEntry {
            video_id: video_id.into(),
            title: Some(title.into()),
            upload_date: Some("20240601".to_string()),
            duration: Some(123.0),
            view_count: Some(1_000),
            channel: Some("Channel One".into()),
            channel_id: Some("UC1".into()),
        }
    }

    async fn prime_channel(pool: &SqlitePool, channel_id: &str) {
        allow_channel(pool, channel_id).await;
        reconcile_with_allowlist(pool).await.unwrap();
    }

    async fn cv_count(pool: &SqlitePool, channel_id: &str, deleted: bool) -> i64 {
        sqlx::query_scalar(
            "SELECT COUNT(*) FROM channel_videos \
              WHERE channel_id = ? AND is_deleted = ?",
        )
        .bind(channel_id)
        .bind(if deleted { 1 } else { 0 })
        .fetch_one(pool)
        .await
        .unwrap()
    }

    #[tokio::test]
    async fn apply_backfill_first_run_inserts_all_rows() {
        let pool = setup_db().await;
        prime_channel(&pool, "UC1").await;
        let bcfg = BackfillConfig::default();
        let entries = vec![entry("vA", "A"), entry("vB", "B"), entry("vC", "C")];

        let started_at = 1_000;
        apply_backfill_entries(&pool, "UC1", started_at, &entries, &bcfg)
            .await
            .unwrap();

        assert_eq!(cv_count(&pool, "UC1", false).await, 3);
        assert_eq!(cv_count(&pool, "UC1", true).await, 0);

        // Every row should have first_seen_at == last_seen_at == started_at,
        // source='backfill', is_deleted=0.
        let rows: Vec<(String, i64, i64, String, i64)> = sqlx::query_as(
            "SELECT video_id, first_seen_at, last_seen_at, source, is_deleted \
               FROM channel_videos WHERE channel_id = 'UC1' ORDER BY video_id",
        )
        .fetch_all(&pool)
        .await
        .unwrap();
        assert_eq!(rows.len(), 3);
        for (_, first, last, source, is_deleted) in &rows {
            assert_eq!(*first, started_at);
            assert_eq!(*last, started_at);
            assert_eq!(source, "backfill");
            assert_eq!(*is_deleted, 0);
        }
    }

    #[tokio::test]
    async fn apply_backfill_second_run_bumps_last_seen_preserves_first_seen() {
        let pool = setup_db().await;
        prime_channel(&pool, "UC1").await;
        let bcfg = BackfillConfig::default();
        let entries = vec![entry("vA", "A"), entry("vB", "B")];

        apply_backfill_entries(&pool, "UC1", 1_000, &entries, &bcfg)
            .await
            .unwrap();
        apply_backfill_entries(&pool, "UC1", 2_500, &entries, &bcfg)
            .await
            .unwrap();

        let (first, last): (i64, i64) = sqlx::query_as(
            "SELECT first_seen_at, last_seen_at FROM channel_videos \
              WHERE channel_id = 'UC1' AND video_id = 'vA'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(first, 1_000, "first_seen_at must be preserved");
        assert_eq!(
            last, 2_500,
            "last_seen_at must be bumped to started_at of latest run"
        );
    }

    #[tokio::test]
    async fn apply_backfill_second_run_tombstones_missing_items() {
        let pool = setup_db().await;
        prime_channel(&pool, "UC1").await;
        let bcfg = BackfillConfig::default();
        let first = vec![entry("vA", "A"), entry("vB", "B"), entry("vC", "C")];
        let second = vec![entry("vA", "A"), entry("vC", "C")]; // vB missing

        apply_backfill_entries(&pool, "UC1", 1_000, &first, &bcfg)
            .await
            .unwrap();
        apply_backfill_entries(&pool, "UC1", 2_000, &second, &bcfg)
            .await
            .unwrap();

        assert_eq!(cv_count(&pool, "UC1", false).await, 2);
        assert_eq!(cv_count(&pool, "UC1", true).await, 1);

        let vb_deleted: i64 = sqlx::query_scalar(
            "SELECT is_deleted FROM channel_videos \
              WHERE channel_id = 'UC1' AND video_id = 'vB'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(vb_deleted, 1, "missing item must be tombstoned");
    }

    #[tokio::test]
    async fn apply_backfill_second_run_inserts_new_items_only() {
        let pool = setup_db().await;
        prime_channel(&pool, "UC1").await;
        let bcfg = BackfillConfig::default();

        apply_backfill_entries(&pool, "UC1", 1_000, &[entry("vA", "A")], &bcfg)
            .await
            .unwrap();
        apply_backfill_entries(
            &pool,
            "UC1",
            2_500,
            &[entry("vA", "A"), entry("vB", "B-new")],
            &bcfg,
        )
        .await
        .unwrap();

        // Existing vA: first_seen_at stays 1000.
        let va_first: i64 = sqlx::query_scalar(
            "SELECT first_seen_at FROM channel_videos \
              WHERE channel_id = 'UC1' AND video_id = 'vA'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(va_first, 1_000);

        // New vB: first_seen_at == 2500 (the second run's started_at).
        let vb_first: i64 = sqlx::query_scalar(
            "SELECT first_seen_at FROM channel_videos \
              WHERE channel_id = 'UC1' AND video_id = 'vB'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(vb_first, 2_500, "new item gets first_seen_at = started_at");
    }

    #[tokio::test]
    async fn apply_backfill_does_not_tombstone_rss_rows_landing_mid_backfill() {
        // The critical safety property: an RSS upsert that lands AFTER
        // a backfill started must not be tombstoned by the
        // reconciliation UPDATE. This guards against the freshness
        // tier racing the reconciliation tier during a long backfill.
        let pool = setup_db().await;
        prime_channel(&pool, "UC1").await;
        let bcfg = BackfillConfig::default();

        // 1. Backfill starts at t=1000 and would observe vA only.
        let started_at = 1_000;
        // 2. Between the backfill's last upsert and its reconciliation
        //    step, an RSS poll lands and writes vB with first_seen_at
        //    AFTER started_at. We simulate this by inserting vB
        //    directly with first_seen_at = 1500 before the call.
        sqlx::query(
            "INSERT INTO channel_videos \
                (channel_id, video_id, title, first_seen_at, last_seen_at, source, is_deleted) \
             VALUES ('UC1', 'vB-from-rss', 'B', ?, ?, 'rss', 0)",
        )
        .bind(1_500)
        .bind(1_500)
        .execute(&pool)
        .await
        .unwrap();

        // 3. The backfill call upserts only vA and runs reconciliation.
        apply_backfill_entries(&pool, "UC1", started_at, &[entry("vA", "A")], &bcfg)
            .await
            .unwrap();

        // The reconciliation MUST NOT tombstone vB-from-rss because
        // its first_seen_at (1500) is NOT < started_at (1000).
        let vb_deleted: i64 = sqlx::query_scalar(
            "SELECT is_deleted FROM channel_videos \
              WHERE channel_id = 'UC1' AND video_id = 'vB-from-rss'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(
            vb_deleted, 0,
            "RSS row with first_seen_at > backfill started_at must NOT be tombstoned"
        );
    }

    #[tokio::test]
    async fn apply_backfill_clears_tombstone_when_video_reappears() {
        // A previously-tombstoned video that reappears in the backfill
        // output should be untombstoned (is_deleted=0) with source
        // flipped back to 'backfill'.
        let pool = setup_db().await;
        prime_channel(&pool, "UC1").await;
        let bcfg = BackfillConfig::default();

        // Seed a tombstoned row from an earlier backfill.
        sqlx::query(
            "INSERT INTO channel_videos \
                (channel_id, video_id, title, first_seen_at, last_seen_at, source, is_deleted) \
             VALUES ('UC1', 'vReborn', 'X', 100, 100, 'backfill', 1)",
        )
        .execute(&pool)
        .await
        .unwrap();

        apply_backfill_entries(&pool, "UC1", 2_000, &[entry("vReborn", "Reborn")], &bcfg)
            .await
            .unwrap();

        let (is_deleted, source, last_seen): (i64, String, i64) = sqlx::query_as(
            "SELECT is_deleted, source, last_seen_at FROM channel_videos \
              WHERE channel_id = 'UC1' AND video_id = 'vReborn'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(is_deleted, 0, "tombstone must be cleared on re-sighting");
        assert_eq!(source, "backfill");
        assert_eq!(last_seen, 2_000);
    }

    #[tokio::test]
    async fn apply_backfill_completion_sets_status_and_next_at() {
        let pool = setup_db().await;
        prime_channel(&pool, "UC1").await;
        let bcfg = BackfillConfig::default();

        let before = unix_now();
        apply_backfill_entries(&pool, "UC1", 1_000, &[entry("vA", "A")], &bcfg)
            .await
            .unwrap();
        let after = unix_now();

        let (status, completed, next, observed, new_last, removed_last): (
            String,
            Option<i64>,
            i64,
            i64,
            i64,
            i64,
        ) = sqlx::query_as(
            "SELECT backfill_status, backfill_last_completed_at, backfill_next_at, \
                    backfill_videos_observed_total, backfill_videos_new_last_run, \
                    backfill_videos_removed_last_run \
               FROM channel_sync_state WHERE channel_id = 'UC1'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        // Successful completion writes 'pending' directly (no
        // intermediate 'complete' status). The completion timestamp +
        // next-at are still set so the loop knows when to re-run.
        assert_eq!(status, "pending");
        assert!(completed.is_some());
        assert!(completed.unwrap() >= before && completed.unwrap() <= after);
        // next_at lands ~30 days in the future (±5%).
        let thirty_days = 30 * 24 * 3600;
        let lower = (thirty_days as f64 * 0.94) as i64 + before;
        let upper = (thirty_days as f64 * 1.06) as i64 + after;
        assert!(
            next >= lower && next <= upper,
            "expected next_at to land near {thirty_days}s in the future, got {next} (window {lower}..={upper})"
        );
        assert_eq!(observed, 1);
        assert_eq!(new_last, 1);
        assert_eq!(removed_last, 0);
    }

    #[tokio::test]
    async fn claim_one_due_recovers_stale_running_lease() {
        // If a worker crashed mid-backfill, the row is stuck with
        // backfill_status='running' and no automatic recovery —
        // unless `claim_one_due` also picks up rows whose
        // `backfill_lease_expires_at` has already passed. This test
        // simulates the crash by writing `'running'` directly with an
        // expired lease, then asserts the next claim reclaims it.
        let pool = setup_db().await;
        allow_channel(&pool, "UCstuck").await;
        reconcile_with_allowlist(&pool).await.unwrap();

        let now = 1_000_000_i64;
        let expired_lease = now - 60; // expired 60s ago
        sqlx::query(
            "UPDATE channel_sync_state SET \
                 backfill_status           = 'running', \
                 backfill_lease_expires_at = ?, \
                 backfill_next_at          = 0 \
              WHERE channel_id = 'UCstuck'",
        )
        .bind(expired_lease)
        .execute(&pool)
        .await
        .unwrap();

        // Without the stale-lease clause this would return None and
        // strand the channel.
        let claimed = claim_one_due(&pool, now).await.unwrap();
        assert_eq!(claimed.as_deref(), Some("UCstuck"));

        // After claim, status is back to 'running' with a fresh lease.
        let (status, lease): (String, Option<i64>) = sqlx::query_as(
            "SELECT backfill_status, backfill_lease_expires_at \
               FROM channel_sync_state WHERE channel_id = 'UCstuck'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(status, "running");
        assert!(
            lease.unwrap() > now,
            "fresh lease must extend into the future"
        );
    }

    #[tokio::test]
    async fn claim_one_due_leaves_running_lease_that_has_not_expired() {
        // The opposite of the stale-lease case: if a worker is
        // actively backfilling (`backfill_lease_expires_at > now`),
        // we must NOT race-claim the channel under it.
        let pool = setup_db().await;
        allow_channel(&pool, "UClive").await;
        reconcile_with_allowlist(&pool).await.unwrap();

        let now = 1_000_000_i64;
        let fresh_lease = now + 30 * 60; // 30 minutes in the future
        sqlx::query(
            "UPDATE channel_sync_state SET \
                 backfill_status           = 'running', \
                 backfill_lease_expires_at = ?, \
                 backfill_next_at          = 0 \
              WHERE channel_id = 'UClive'",
        )
        .bind(fresh_lease)
        .execute(&pool)
        .await
        .unwrap();

        let claimed = claim_one_due(&pool, now).await.unwrap();
        assert_eq!(
            claimed, None,
            "fresh-lease running channel must not be reclaimed"
        );
    }

    #[tokio::test]
    async fn claim_one_due_skips_channel_with_recent_sidecar_fallback() {
        // Anti-bot safeguard #7 from the plan: a channel whose
        // freshness loop just took a sidecar fallback must NOT be
        // claimed by the backfill loop until the cooldown elapses.
        let pool = setup_db().await;
        allow_channel(&pool, "UCcool").await;
        reconcile_with_allowlist(&pool).await.unwrap();

        // Pretend the freshness loop took a sidecar fallback 10
        // minutes ago — well inside the 1h cooldown.
        let now: i64 = 1_000_000;
        let recent = now - 10 * 60;
        sqlx::query(
            "UPDATE channel_sync_state SET last_sidecar_fallback_at = ? \
              WHERE channel_id = 'UCcool'",
        )
        .bind(recent)
        .execute(&pool)
        .await
        .unwrap();

        // Even though the row is otherwise eligible
        // (backfill_status='pending' AND backfill_next_at=0), the
        // cooldown gates it out.
        let claimed = claim_one_due(&pool, now).await.unwrap();
        assert_eq!(
            claimed, None,
            "channel within sidecar cooldown must not be claimed"
        );

        // After the cooldown elapses, the channel becomes claimable.
        let later = now + SIDECAR_FALLBACK_COOLDOWN_SECS + 1;
        let claimed = claim_one_due(&pool, later).await.unwrap();
        assert_eq!(
            claimed.as_deref(),
            Some("UCcool"),
            "channel becomes claimable after the sidecar cooldown elapses"
        );
    }

    #[tokio::test]
    async fn claim_one_due_does_not_block_channels_that_never_used_sidecar() {
        // Symmetric to the above: channels with NULL
        // last_sidecar_fallback_at (i.e. RSS has always succeeded) are
        // not gated by the cooldown.
        let pool = setup_db().await;
        allow_channel(&pool, "UCnever").await;
        reconcile_with_allowlist(&pool).await.unwrap();

        let claimed = claim_one_due(&pool, 1_000_000).await.unwrap();
        assert_eq!(claimed.as_deref(), Some("UCnever"));
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

    // -----------------------------------------------------------------
    // record_failure — failure classification + state transitions.
    // Three branches: channel-not-found shelve, consecutive-errors
    // shelve, and normal exponential backoff.
    // -----------------------------------------------------------------

    #[tokio::test]
    async fn record_failure_shelves_immediately_on_not_found() {
        let pool = setup_db().await;
        allow_channel(&pool, "UCgone").await;
        reconcile_with_allowlist(&pool).await.unwrap();
        // Pretend the row was just claimed.
        sqlx::query(
            "UPDATE channel_sync_state SET \
                 backfill_status = 'running', \
                 backfill_lease_expires_at = 9999999 \
             WHERE channel_id = 'UCgone'",
        )
        .execute(&pool)
        .await
        .unwrap();

        let bcfg = BackfillConfig::default();
        // A "channel does not exist" error — should single-strike shelve.
        record_failure(&pool, "UCgone", "This channel does not exist", &bcfg)
            .await
            .unwrap();

        let (status, err, lease): (String, Option<String>, Option<i64>) = sqlx::query_as(
            "SELECT backfill_status, backfill_last_error, backfill_lease_expires_at \
               FROM channel_sync_state WHERE channel_id = 'UCgone'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(status, "shelved");
        assert_eq!(err.as_deref(), Some("channel_not_found"));
        assert!(lease.is_none(), "lease must be cleared on shelve");
    }

    #[tokio::test]
    async fn record_failure_shelves_after_max_consecutive_errors() {
        let pool = setup_db().await;
        allow_channel(&pool, "UCflaky").await;
        reconcile_with_allowlist(&pool).await.unwrap();
        let bcfg = BackfillConfig {
            max_consecutive_errors_before_shelve: 3,
            ..BackfillConfig::default()
        };
        // Seed the channel right at the threshold-minus-one so the
        // next failure trips the shelve.
        sqlx::query(
            "UPDATE channel_sync_state SET \
                 backfill_status = 'running', \
                 backfill_consecutive_errors = 2 \
             WHERE channel_id = 'UCflaky'",
        )
        .execute(&pool)
        .await
        .unwrap();

        record_failure(&pool, "UCflaky", "transient error", &bcfg)
            .await
            .unwrap();

        let (status, errs, err): (String, i64, Option<String>) = sqlx::query_as(
            "SELECT backfill_status, backfill_consecutive_errors, backfill_last_error \
               FROM channel_sync_state WHERE channel_id = 'UCflaky'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(status, "shelved");
        assert_eq!(errs, 3, "counter is bumped before the threshold check");
        assert_eq!(err.as_deref(), Some("transient error"));
    }

    #[tokio::test]
    async fn record_failure_backs_off_under_threshold() {
        let pool = setup_db().await;
        allow_channel(&pool, "UCsoft").await;
        reconcile_with_allowlist(&pool).await.unwrap();
        let bcfg = BackfillConfig {
            max_consecutive_errors_before_shelve: 5,
            ..BackfillConfig::default()
        };
        sqlx::query(
            "UPDATE channel_sync_state SET backfill_status = 'running' \
              WHERE channel_id = 'UCsoft'",
        )
        .execute(&pool)
        .await
        .unwrap();

        let before = unix_now();
        record_failure(&pool, "UCsoft", "transient", &bcfg)
            .await
            .unwrap();
        let after = unix_now();

        let (status, errs, next, lease): (String, i64, i64, Option<i64>) = sqlx::query_as(
            "SELECT backfill_status, backfill_consecutive_errors, backfill_next_at, \
                    backfill_lease_expires_at \
               FROM channel_sync_state WHERE channel_id = 'UCsoft'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(status, "pending", "row goes back to 'pending' for retry");
        assert_eq!(errs, 1);
        assert!(lease.is_none(), "lease cleared on backoff");
        // backoff_for_attempt(1, 3600s) computes
        // 2 ^ clamp(1, 1, 16) × 3600 = 2 × 3600 = 7200, then applies
        // ±15% jitter, so the value lands in roughly [6120, 8280].
        // Lower bound: be defensive against any rounding in
        // jittered_interval — use 6000 as the floor (giving ~120s
        // slack on the 15% nominal).
        let backoff_lower = 6_000_i64;
        let backoff_upper = 8_500_i64;
        assert!(
            next >= before + backoff_lower,
            "next_at must be at least ~6000s in the future (got {next}, before={before})"
        );
        assert!(
            next <= after + backoff_upper,
            "next_at must be at most ~8500s in the future (got {next}, after={after})"
        );
    }

    #[test]
    fn is_not_found_error_classifies_404_signals() {
        // Real yt-dlp / HTTP messages that genuinely indicate a
        // permanently-gone channel.
        assert!(is_not_found_error("HTTP Error 404: Not Found"));
        assert!(is_not_found_error("This channel does not exist"));
        assert!(is_not_found_error("This channel was terminated"));
        assert!(is_not_found_error("server returned 404 not found"));

        // Negatives: transient errors that contain similar words but
        // must NOT shelve the channel for a year. Each of these was
        // matched by the prior overly-broad classifier.
        assert!(
            !is_not_found_error("Sign in to confirm you're not a bot"),
            "bot-wall messages must not trigger not-found"
        );
        assert!(!is_not_found_error("rate-limit"));
        assert!(
            !is_not_found_error("YouTube said: The service is unavailable"),
            "transient 'unavailable' must not trigger not-found"
        );
        assert!(
            !is_not_found_error("Video unavailable: This video is unavailable in your country"),
            "geo-blocked 'unavailable' must not trigger not-found"
        );
        assert!(
            !is_not_found_error("requested format not found"),
            "extractor-level 'not found' must not match"
        );
        assert!(
            !is_not_found_error("https://example.com/some/path/404page.html"),
            "incidental 404 inside a URL must not match"
        );
        assert!(
            !is_not_found_error("Error fetching video 4042: timeout"),
            "incidental 404 inside numeric content must not match"
        );
    }

    #[test]
    fn is_bot_check_error_classifies_well_known_signals() {
        // Realistic yt-dlp / HTTP messages that genuinely indicate a
        // bot-wall block.
        assert!(is_bot_check_error("Sign in to confirm you're not a bot"));
        assert!(is_bot_check_error("HTTP Error 429: Too Many Requests"));
        assert!(is_bot_check_error("HTTP Error 403: Forbidden"));
        assert!(is_bot_check_error("server returned status 429"));
        assert!(is_bot_check_error("consent.youtube.com redirect"));
        assert!(is_bot_check_error("captcha required"));
        assert!(is_bot_check_error("rate-limit exceeded"));

        // Negatives: messages that contain similar-looking words but
        // are NOT bot walls.
        assert!(!is_bot_check_error("HTTP Error 404"));
        assert!(!is_bot_check_error(
            "Error 429 ms elapsed during DNS lookup"
        ));
        assert!(!is_bot_check_error(
            "https://example.com/api/v1/403/something"
        ));
    }
}
