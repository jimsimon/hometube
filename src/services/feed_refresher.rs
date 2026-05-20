//! Background task that keeps `feed_source_items` warm.
//!
//! Picks the most-overdue sources, polls them (currently RSS-only —
//! channels), and writes the results back via [`feed_cache`]. Drives
//! the user-facing `/api/feed/new-videos` endpoint without it ever
//! having to talk to YouTube.
//!
//! ### Concurrency and rate limiting
//!
//! Two knobs, both static for now (tunable via `app_config` later if
//! we need to):
//!
//! - **Global RPS gate.** Between dispatches we sleep at least
//!   `1.0 / GLOBAL_RPS` seconds so no matter how many sources are
//!   overdue we don't exceed the configured request rate.
//! - **Max inflight.** A `tokio::sync::Semaphore` caps how many polls
//!   are awaiting a response at any moment.
//!
//! ### Scheduling
//!
//! After a successful poll: `next_poll_at = now + CHANNEL_INTERVAL ± 15% jitter`.
//! After a failure: exponential backoff
//! `min(MAX_BACKOFF, CHANNEL_INTERVAL * 2^errors)`, also jittered.
//!
//! ### Lifecycle
//!
//! Spawned from `main` after the cron scheduler. The task loops
//! forever; on shutdown the runtime drops it.

use std::time::Duration;

use futures_util::stream::{FuturesUnordered, StreamExt};
use rand::RngExt;
use reqwest::Client;
use sqlx::SqlitePool;
use tokio::time::{interval, MissedTickBehavior};
use tracing::{debug, info, warn};

use crate::services::feed_cache::{self, DueSource, ItemRow, KIND_CHANNEL};
use crate::services::youtube_rss::{self, PollOutcome};

// ---------------------------------------------------------------------------
// Tunable defaults
// ---------------------------------------------------------------------------
//
// Each value can be overridden at runtime via an `app_config` key (see
// the `KEY_*` constants below). The refresher reads them once per tick
// so changes take effect within `IDLE_TICK` seconds with no restart.

/// Steady-state interval between polls of a successfully-fetched
/// channel.
pub const DEFAULT_CHANNEL_INTERVAL_S: u64 = 60 * 60;

/// Cap on the exponential-backoff interval after repeated failures.
pub const MAX_BACKOFF: Duration = Duration::from_secs(24 * 60 * 60);

/// Sleep this long when `claim_due_sources` returns nothing.
pub const DEFAULT_IDLE_TICK_S: u64 = 30;

/// Number of overdue sources to claim per loop iteration.
pub const DEFAULT_BATCH_SIZE: i64 = 25;

/// At most this many polls in flight concurrently across the task.
pub const DEFAULT_MAX_INFLIGHT: usize = 4;

/// Minimum delay between dispatches — caps the global request rate at
/// ~1 / `DISPATCH_DELAY` requests per second. Enforced by a
/// `tokio::time::interval` so MAX_INFLIGHT completing in a burst
/// cannot bypass the rate gate.
pub const DEFAULT_DISPATCH_DELAY_MS: u64 = 2_000;

/// How long a claimed source stays "leased" before another claim can
/// pick it up again. Chosen comfortably larger than the RSS request
/// timeout so the lease only ever expires when the worker has actually
/// crashed without writing back. Not surfaced as a tunable — purely
/// derived from operational invariants.
pub const LEASE_SECS: i64 = 120;

// `app_config` keys for the live tunables.
pub const KEY_DISPATCH_DELAY_MS: &str = "feed_refresher_dispatch_delay_ms";
pub const KEY_MAX_INFLIGHT: &str = "feed_refresher_max_inflight";
pub const KEY_BATCH_SIZE: &str = "feed_refresher_batch_size";
pub const KEY_IDLE_TICK_S: &str = "feed_refresher_idle_tick_s";
pub const KEY_CHANNEL_INTERVAL_S: &str = "feed_channel_interval_s";

/// A snapshot of the live refresher knobs, taken once per outer-loop
/// iteration. Bad/missing values silently fall back to the defaults so
/// the refresher cannot be wedged by a bad config write.
#[derive(Debug, Clone, Copy)]
pub struct RefresherConfig {
    pub dispatch_delay: Duration,
    pub max_inflight: usize,
    pub batch_size: i64,
    pub idle_tick: Duration,
    pub channel_interval: Duration,
}

impl Default for RefresherConfig {
    fn default() -> Self {
        Self {
            dispatch_delay: Duration::from_millis(DEFAULT_DISPATCH_DELAY_MS),
            max_inflight: DEFAULT_MAX_INFLIGHT,
            batch_size: DEFAULT_BATCH_SIZE,
            idle_tick: Duration::from_secs(DEFAULT_IDLE_TICK_S),
            channel_interval: Duration::from_secs(DEFAULT_CHANNEL_INTERVAL_S),
        }
    }
}

/// Raw `app_config` values for the refresher tunables. Returned by
/// [`RefresherConfig::load_raw`] alongside the effective [`RefresherConfig`]
/// so the diagnostics UI can warn the operator when a stored value
/// was clamped out by range validation.
#[derive(Debug, Clone, Default)]
pub struct RefresherConfigRaw {
    pub dispatch_delay_ms: Option<String>,
    pub max_inflight: Option<String>,
    pub batch_size: Option<String>,
    pub idle_tick_s: Option<String>,
    pub channel_interval_s: Option<String>,
}

impl RefresherConfig {
    /// Load and validate the effective config. Bad/missing values
    /// silently fall back to defaults.
    pub async fn load(pool: &SqlitePool) -> Self {
        let (cfg, _) = Self::load_with_raw(pool).await;
        cfg
    }

    /// Load the effective config and the raw values from `app_config`
    /// in a single query. The caller can compare effective vs raw to
    /// detect (and warn about) out-of-range stored values.
    pub async fn load_with_raw(pool: &SqlitePool) -> (Self, RefresherConfigRaw) {
        let mut raw = RefresherConfigRaw::default();
        // Single batched query rather than five separate SELECTs.
        let rows = sqlx::query_as::<_, (String, String)>(
            "SELECT key, value FROM app_config WHERE key IN (?, ?, ?, ?, ?)",
        )
        .bind(KEY_DISPATCH_DELAY_MS)
        .bind(KEY_MAX_INFLIGHT)
        .bind(KEY_BATCH_SIZE)
        .bind(KEY_IDLE_TICK_S)
        .bind(KEY_CHANNEL_INTERVAL_S)
        .fetch_all(pool)
        .await
        .unwrap_or_default();
        for (k, v) in rows {
            match k.as_str() {
                KEY_DISPATCH_DELAY_MS => raw.dispatch_delay_ms = Some(v),
                KEY_MAX_INFLIGHT => raw.max_inflight = Some(v),
                KEY_BATCH_SIZE => raw.batch_size = Some(v),
                KEY_IDLE_TICK_S => raw.idle_tick_s = Some(v),
                KEY_CHANNEL_INTERVAL_S => raw.channel_interval_s = Some(v),
                _ => {}
            }
        }

        // Range checks here are the canonical source of truth and are
        // mirrored by the PUT-endpoint validator. Out-of-range values
        // silently fall back to the default so the refresher cannot
        // be wedged by a bad config write.
        let mut cfg = RefresherConfig::default();
        if let Some(v) = raw
            .dispatch_delay_ms
            .as_deref()
            .and_then(|s| s.parse::<u64>().ok())
        {
            if RANGE_DISPATCH_DELAY_MS.contains(&v) {
                cfg.dispatch_delay = Duration::from_millis(v);
            }
        }
        if let Some(v) = raw
            .max_inflight
            .as_deref()
            .and_then(|s| s.parse::<u64>().ok())
        {
            if RANGE_MAX_INFLIGHT.contains(&v) {
                cfg.max_inflight = v as usize;
            }
        }
        if let Some(v) = raw
            .batch_size
            .as_deref()
            .and_then(|s| s.parse::<i64>().ok())
        {
            if RANGE_BATCH_SIZE.contains(&v) {
                cfg.batch_size = v;
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
        if let Some(v) = raw
            .channel_interval_s
            .as_deref()
            .and_then(|s| s.parse::<u64>().ok())
        {
            if RANGE_CHANNEL_INTERVAL_S.contains(&v) {
                cfg.channel_interval = Duration::from_secs(v);
            }
        }
        (cfg, raw)
    }
}

// Canonical (inclusive) ranges for each tunable. Used both by
// `RefresherConfig::load_with_raw` (to clamp values from app_config)
// and re-exported for the PUT-endpoint validator so the two cannot
// drift apart.
pub const RANGE_DISPATCH_DELAY_MS: std::ops::RangeInclusive<u64> = 50..=600_000;
pub const RANGE_MAX_INFLIGHT: std::ops::RangeInclusive<u64> = 1..=64;
pub const RANGE_BATCH_SIZE: std::ops::RangeInclusive<i64> = 1..=500;
pub const RANGE_IDLE_TICK_S: std::ops::RangeInclusive<u64> = 1..=3600;
pub const RANGE_CHANNEL_INTERVAL_S: std::ops::RangeInclusive<u64> = 60..=86_400;

/// Public entry point: spawn the refresher onto the current runtime.
/// Hands back a `JoinHandle` purely for testability; production code
/// just discards it.
pub fn spawn(pool: SqlitePool) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        run(pool).await;
    })
}

/// Background refresher loop. Exposed (and hidden from rustdoc) only
/// so integration tests can spawn it directly and abort the resulting
/// `JoinHandle` rather than going through [`spawn`] (which discards
/// the handle). Production callers should use [`spawn`].
#[doc(hidden)]
pub async fn run(pool: SqlitePool) {
    // Builder failure is treated as fatal for the refresher: if we
    // can't construct a properly-configured HTTP client we won't run
    // at all, rather than silently fall back to a client with no
    // timeout that could hang one of the few inflight slots forever.
    let http = match Client::builder()
        .user_agent("hometube/0.1 (+rss)")
        .timeout(Duration::from_secs(30))
        .build()
    {
        Ok(c) => c,
        Err(err) => {
            warn!(%err, "feed refresher: HTTP client construction failed; refresher disabled");
            return;
        }
    };

    info!("feed refresher starting");

    let mut inflight: FuturesUnordered<tokio::task::JoinHandle<()>> = FuturesUnordered::new();

    loop {
        // Re-read tunables every tick. Mis-typed / out-of-range values
        // silently fall back to defaults inside `RefresherConfig::load`.
        let cfg = RefresherConfig::load(&pool).await;

        // Bounded by cfg.max_inflight polls and gated by
        // cfg.dispatch_delay between dispatches. `MissedTickBehavior::Delay`
        // ensures that if the tick fires while we're blocked on
        // backpressure, the next tick simply schedules one dispatch_delay
        // into the future rather than firing immediately — so completing
        // many polls in a burst cannot release many new requests at
        // full speed.
        let mut tick = interval(cfg.dispatch_delay);
        tick.set_missed_tick_behavior(MissedTickBehavior::Delay);

        let now = unix_now();
        // Atomically claim a batch: the lease pushes each row's
        // next_poll_at into the future so the next claim_due_sources
        // call (in this iteration or another) cannot return the same
        // rows while their polls are still in flight.
        let due = match feed_cache::claim_due_sources(&pool, now, cfg.batch_size, LEASE_SECS).await
        {
            Ok(d) => d,
            Err(err) => {
                warn!(%err, "feed refresher: claim_due_sources query failed");
                tokio::time::sleep(cfg.idle_tick).await;
                continue;
            }
        };

        if due.is_empty() {
            tokio::time::sleep(cfg.idle_tick).await;
            continue;
        }

        let span = tracing::info_span!(
            "feed.refresher.tick",
            sources = due.len(),
            max_inflight = cfg.max_inflight,
            dispatch_delay_ms = cfg.dispatch_delay.as_millis() as u64,
        );
        let _enter = span.enter();
        debug!("dispatching polls");
        let base = youtube_rss::base_url(&pool).await;

        for source in due {
            // Apply backpressure: never have more than cfg.max_inflight
            // polls running concurrently.
            while inflight.len() >= cfg.max_inflight {
                inflight.next().await;
            }
            // Apply rate-limit gate: at most one dispatch per
            // cfg.dispatch_delay across the lifetime of the loop body.
            tick.tick().await;

            let pool = pool.clone();
            let http = http.clone();
            let base = base.clone();

            inflight.push(tokio::spawn(async move {
                if let Err(err) = poll_one(&pool, &http, &base, &source, cfg).await {
                    warn!(
                        kind = %source.kind,
                        source_id = %source.source_id,
                        %err,
                        "feed refresher: poll task errored at outer layer",
                    );
                }
            }));
        }

        // Drain the in-flight set before re-querying for due sources.
        // This means the worst-case latency between a source becoming
        // due and being polled is one batch (≤ batch_size * dispatch_delay),
        // which at the default 25 × 2 s = 50 s is well below the
        // 1-hour interval.
        while inflight.next().await.is_some() {}
    }
}

/// Drive one source through poll + persist. Errors here are logged but
/// swallowed by the outer caller; per-source failures are *also*
/// recorded in `feed_sources.last_error` for diagnostics.
#[tracing::instrument(
    name = "feed.poll",
    skip_all,
    fields(kind = %source.kind, source_id = %source.source_id),
)]
async fn poll_one(
    pool: &SqlitePool,
    http: &Client,
    base: &str,
    source: &DueSource,
    cfg: RefresherConfig,
) -> crate::error::AppResult<()> {
    if source.kind != KIND_CHANNEL {
        // Playlists are deferred. Reschedule far into the future
        // (1 year) via the *skipped* path so we:
        //   (a) don't accumulate consecutive_errors forever, and
        //   (b) don't pollute last_polled_at / last_success_at on
        //       the diagnostics page with a "healthy" timestamp for
        //       a row that never made a network request.
        let one_year_secs: i64 = 365 * 24 * 60 * 60;
        feed_cache::record_poll_skipped(
            pool,
            &source.kind,
            &source.source_id,
            "kind not supported (deferred)",
            unix_now() + one_year_secs,
        )
        .await?;
        return Ok(());
    }

    let now = unix_now();
    let result = youtube_rss::poll_channel(
        http,
        base,
        &source.source_id,
        source.etag.as_deref(),
        source.last_modified.as_deref(),
    )
    .await;

    match result {
        Ok(PollOutcome::NotModified) => {
            let next = now + jittered_interval(cfg.channel_interval);
            feed_cache::record_poll_success(
                pool,
                feed_cache::PollSuccess {
                    kind: &source.kind,
                    source_id: &source.source_id,
                    title: None,
                    etag: source.etag.as_deref(),
                    last_modified: source.last_modified.as_deref(),
                    next_poll_at: next,
                    now,
                },
            )
            .await?;
        }
        Ok(PollOutcome::Updated {
            title,
            etag,
            last_modified,
            items,
        }) => {
            persist_items(pool, &source.kind, &source.source_id, &items, now).await?;
            let next = now + jittered_interval(cfg.channel_interval);
            feed_cache::record_poll_success(
                pool,
                feed_cache::PollSuccess {
                    kind: &source.kind,
                    source_id: &source.source_id,
                    title: title.as_deref(),
                    etag: etag.as_deref(),
                    last_modified: last_modified.as_deref(),
                    next_poll_at: next,
                    now,
                },
            )
            .await?;
            debug!(count = items.len(), "source updated");
        }
        Err(err) => {
            let next =
                now + backoff_for_attempt(source.consecutive_errors + 1, cfg.channel_interval);
            let msg = err.to_string();
            feed_cache::record_poll_failure(pool, &source.kind, &source.source_id, &msg, next, now)
                .await?;
            warn!(error = %msg, "source poll failed");
        }
    }
    Ok(())
}

async fn persist_items(
    pool: &SqlitePool,
    kind: &str,
    source_id: &str,
    items: &[ItemRow],
    now: i64,
) -> crate::error::AppResult<()> {
    feed_cache::replace_source_items(pool, kind, source_id, items, now).await
}

/// `interval ± 15%`, as seconds.
pub fn jittered_interval(interval: Duration) -> i64 {
    let secs = interval.as_secs() as f64;
    let jitter_frac: f64 = rand::rng().random_range(-0.15..0.15);
    (secs + secs * jitter_frac).round() as i64
}

/// Exponential backoff with jitter, capped at [`MAX_BACKOFF`].
///
/// `attempt` is the post-failure error count (i.e. 1 for the first
/// failure). `interval` is the steady-state inter-poll interval used
/// as the geometric base. The first failed attempt waits 2× the
/// steady-state interval (not 1×, which would have been
/// indistinguishable from a healthy poll cadence — the function name
/// implies escalation, so each attempt actually escalates).
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

    const TEST_INTERVAL: Duration = Duration::from_secs(DEFAULT_CHANNEL_INTERVAL_S);

    #[test]
    fn jitter_stays_within_band() {
        for _ in 0..200 {
            let v = jittered_interval(TEST_INTERVAL);
            let base = TEST_INTERVAL.as_secs() as f64;
            assert!(
                (v as f64) >= base * 0.85 - 1.0 && (v as f64) <= base * 1.15 + 1.0,
                "out of band: {v}",
            );
        }
    }

    #[test]
    fn backoff_grows_then_caps() {
        let one = backoff_for_attempt(1, TEST_INTERVAL);
        let two = backoff_for_attempt(2, TEST_INTERVAL);
        let three = backoff_for_attempt(3, TEST_INTERVAL);
        let huge = backoff_for_attempt(50, TEST_INTERVAL);
        // attempt=1 must escalate past the steady-state interval — no
        // off-by-one with healthy-poll cadence.
        let base = TEST_INTERVAL.as_secs() as f64;
        assert!((one as f64) >= base * 1.5, "first backoff too short: {one}");
        assert!(one < two);
        assert!(two < three);
        // Eventually we hit the 24h cap.
        let cap = MAX_BACKOFF.as_secs() as i64;
        assert!(huge <= (cap as f64 * 1.16) as i64);
        assert!(huge >= (cap as f64 * 0.84) as i64);
    }
}
