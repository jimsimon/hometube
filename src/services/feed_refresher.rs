//! Background task that keeps `channel_videos` warm for the freshness
//! tier (RSS → InnerTube sidecar fallback).
//!
//! Picks the most-overdue channels, polls them via RSS (with sidecar
//! fallback on RSS failure), and writes the results back via
//! [`feed_cache`]. Drives the user-facing `/api/feed/new-videos`
//! endpoint without it ever having to talk to YouTube on the request
//! path.
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

use crate::services::feed_cache::{self, DueSource, ItemRow};
use crate::services::youtube::{
    ChannelVideoItem as SidecarChannelVideoItem, SidecarRefresherOutcome, YoutubeClient,
};
use crate::services::youtube_rss::{self, PollOutcome};

/// Sanity ceiling on the per-channel sidecar fallback response — caps
/// a runaway InnerTube response. Matches the natural ~15-item ceiling
/// of the RSS feed so RSS and sidecar produce comparably-sized writes.
/// Vestigial since the per-source storage cap was removed in the
/// channel_videos consolidation, but retained as a defensive ceiling.
const SIDECAR_FALLBACK_MAX_ITEMS: u32 = 15;

/// When the sidecar confirms a source is dead (`NotFound`), push its
/// `next_poll_at` this far into the future. The row is preserved so a
/// future reactivation (channel restored, or the operator manually
/// resets `next_poll_at`) can pick it back up.
const DEAD_SOURCE_DEFER_SECS: i64 = 365 * 24 * 60 * 60;

/// How many consecutive sidecar `NotFound` responses we require before
/// shelving a source as dead. A single 404 isn't enough because
/// channels can briefly 404 during YouTube-side glitches. Each preliminary 404
/// records a normal poll-failure (incrementing `consecutive_errors`
/// and applying exponential backoff), so the source gets retried
/// progressively less often until either it recovers or we've seen
/// enough 404s in a row to be confident the source really is gone.
const SIDECAR_NOTFOUND_SHELVE_THRESHOLD: i64 = 3;

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

/// Whether the refresher is allowed to fall back to the youtubei.js
/// discovery sidecar when an RSS poll fails. The fallback (a) keeps
/// the new-videos feed fresh during YouTube's intermittent RSS
/// outages, and (b) classifies sources as dead vs. temporarily
/// unreachable (something RSS can't disambiguate — both surface as a
/// bare 404). The operator can flip this off as a kill switch if the
/// sidecar starts misbehaving without restarting the process.
pub const DEFAULT_SIDECAR_FALLBACK_ENABLED: bool = true;

/// Per-source rate cap on sidecar fallbacks for an *active* channel
/// (most recent upload within the dormant threshold). The refresher
/// will not issue a second fallback for the same channel until at
/// least this long has passed since the last one.
pub const DEFAULT_SIDECAR_FALLBACK_MIN_INTERVAL_S: u64 = 60 * 60;

/// Per-source rate cap on sidecar fallbacks for a *dormant* channel
/// (no uploads in the last `sidecar_dormant_threshold_days` but within
/// `sidecar_archived_threshold_days`). 6 hours by default — a dormant
/// channel hasn't uploaded recently, so RSS-vs-sidecar freshness
/// matters less.
pub const DEFAULT_SIDECAR_FALLBACK_DORMANT_INTERVAL_S: u64 = 6 * 60 * 60;

/// Per-source rate cap on sidecar fallbacks for an *archived* channel
/// (no uploads in `sidecar_archived_threshold_days`, or no uploads
/// ever observed). 24 hours by default — the channel has gone weeks+
/// without anything new, so the riskier transport can wait.
pub const DEFAULT_SIDECAR_FALLBACK_ARCHIVED_INTERVAL_S: u64 = 24 * 60 * 60;

/// Days since the most-recent upload before a channel transitions
/// from "active" to "dormant" (the second-tier interval kicks in).
pub const DEFAULT_SIDECAR_DORMANT_THRESHOLD_DAYS: u64 = 30;

/// Days since the most-recent upload before a channel transitions
/// from "dormant" to "archived" (the third-tier interval kicks in).
pub const DEFAULT_SIDECAR_ARCHIVED_THRESHOLD_DAYS: u64 = 90;

/// Aggregate per-hour cap on sidecar fallbacks across the entire
/// refresher. `0` means "no aggregate cap" (the per-source cap still
/// applies). Default of 120/hour comfortably absorbs a small home
/// install during an outage while putting a ceiling on large
/// deployments (hundreds of channels) where the per-source cap alone
/// would still allow a high aggregate call rate.
pub const DEFAULT_SIDECAR_FALLBACK_MAX_PER_HOUR: u64 = 120;

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
pub const KEY_SIDECAR_FALLBACK_ENABLED: &str = "feed_refresher_sidecar_fallback_enabled";
/// Per-source min interval for an *active* channel (≤dormant_threshold_days
/// since last upload). Backward-compatible with the pre-adaptive setting
/// — operators who hand-set this still get the active-channel behaviour.
pub const KEY_SIDECAR_FALLBACK_MIN_INTERVAL_S: &str =
    "feed_refresher_sidecar_fallback_min_interval_s";
pub const KEY_SIDECAR_FALLBACK_DORMANT_INTERVAL_S: &str =
    "feed_refresher_sidecar_fallback_dormant_interval_s";
pub const KEY_SIDECAR_FALLBACK_ARCHIVED_INTERVAL_S: &str =
    "feed_refresher_sidecar_fallback_archived_interval_s";
pub const KEY_SIDECAR_DORMANT_THRESHOLD_DAYS: &str =
    "feed_refresher_sidecar_dormant_threshold_days";
pub const KEY_SIDECAR_ARCHIVED_THRESHOLD_DAYS: &str =
    "feed_refresher_sidecar_archived_threshold_days";
pub const KEY_SIDECAR_FALLBACK_MAX_PER_HOUR: &str = "feed_refresher_sidecar_fallback_max_per_hour";

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
    /// See [`DEFAULT_SIDECAR_FALLBACK_ENABLED`].
    pub sidecar_fallback_enabled: bool,
    /// Per-source min interval for an *active* channel.
    pub sidecar_fallback_min_interval: Duration,
    /// Per-source min interval for a *dormant* channel.
    pub sidecar_fallback_dormant_interval: Duration,
    /// Per-source min interval for an *archived* channel.
    pub sidecar_fallback_archived_interval: Duration,
    /// Days since last upload before "active" → "dormant".
    pub sidecar_dormant_threshold_days: u64,
    /// Days since last upload before "dormant" → "archived".
    pub sidecar_archived_threshold_days: u64,
    /// See [`DEFAULT_SIDECAR_FALLBACK_MAX_PER_HOUR`]. `0` means "no
    /// aggregate cap".
    pub sidecar_fallback_max_per_hour: u64,
}

impl Default for RefresherConfig {
    fn default() -> Self {
        Self {
            dispatch_delay: Duration::from_millis(DEFAULT_DISPATCH_DELAY_MS),
            max_inflight: DEFAULT_MAX_INFLIGHT,
            batch_size: DEFAULT_BATCH_SIZE,
            idle_tick: Duration::from_secs(DEFAULT_IDLE_TICK_S),
            channel_interval: Duration::from_secs(DEFAULT_CHANNEL_INTERVAL_S),
            sidecar_fallback_enabled: DEFAULT_SIDECAR_FALLBACK_ENABLED,
            sidecar_fallback_min_interval: Duration::from_secs(
                DEFAULT_SIDECAR_FALLBACK_MIN_INTERVAL_S,
            ),
            sidecar_fallback_dormant_interval: Duration::from_secs(
                DEFAULT_SIDECAR_FALLBACK_DORMANT_INTERVAL_S,
            ),
            sidecar_fallback_archived_interval: Duration::from_secs(
                DEFAULT_SIDECAR_FALLBACK_ARCHIVED_INTERVAL_S,
            ),
            sidecar_dormant_threshold_days: DEFAULT_SIDECAR_DORMANT_THRESHOLD_DAYS,
            sidecar_archived_threshold_days: DEFAULT_SIDECAR_ARCHIVED_THRESHOLD_DAYS,
            sidecar_fallback_max_per_hour: DEFAULT_SIDECAR_FALLBACK_MAX_PER_HOUR,
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
    pub sidecar_fallback_enabled: Option<String>,
    pub sidecar_fallback_min_interval_s: Option<String>,
    pub sidecar_fallback_dormant_interval_s: Option<String>,
    pub sidecar_fallback_archived_interval_s: Option<String>,
    pub sidecar_dormant_threshold_days: Option<String>,
    pub sidecar_archived_threshold_days: Option<String>,
    pub sidecar_fallback_max_per_hour: Option<String>,
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
        // Single batched query rather than separate SELECTs.
        let rows = sqlx::query_as::<_, (String, String)>(
            "SELECT key, value FROM app_config \
             WHERE key IN (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(KEY_DISPATCH_DELAY_MS)
        .bind(KEY_MAX_INFLIGHT)
        .bind(KEY_BATCH_SIZE)
        .bind(KEY_IDLE_TICK_S)
        .bind(KEY_CHANNEL_INTERVAL_S)
        .bind(KEY_SIDECAR_FALLBACK_ENABLED)
        .bind(KEY_SIDECAR_FALLBACK_MIN_INTERVAL_S)
        .bind(KEY_SIDECAR_FALLBACK_DORMANT_INTERVAL_S)
        .bind(KEY_SIDECAR_FALLBACK_ARCHIVED_INTERVAL_S)
        .bind(KEY_SIDECAR_DORMANT_THRESHOLD_DAYS)
        .bind(KEY_SIDECAR_ARCHIVED_THRESHOLD_DAYS)
        .bind(KEY_SIDECAR_FALLBACK_MAX_PER_HOUR)
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
                KEY_SIDECAR_FALLBACK_ENABLED => raw.sidecar_fallback_enabled = Some(v),
                KEY_SIDECAR_FALLBACK_MIN_INTERVAL_S => {
                    raw.sidecar_fallback_min_interval_s = Some(v)
                }
                KEY_SIDECAR_FALLBACK_DORMANT_INTERVAL_S => {
                    raw.sidecar_fallback_dormant_interval_s = Some(v)
                }
                KEY_SIDECAR_FALLBACK_ARCHIVED_INTERVAL_S => {
                    raw.sidecar_fallback_archived_interval_s = Some(v)
                }
                KEY_SIDECAR_DORMANT_THRESHOLD_DAYS => raw.sidecar_dormant_threshold_days = Some(v),
                KEY_SIDECAR_ARCHIVED_THRESHOLD_DAYS => {
                    raw.sidecar_archived_threshold_days = Some(v)
                }
                KEY_SIDECAR_FALLBACK_MAX_PER_HOUR => raw.sidecar_fallback_max_per_hour = Some(v),
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
        // Sidecar-fallback toggles. The enable flag accepts the same
        // truthy strings as the parent settings UI sends ("true" /
        // "false"); anything else falls back to the default.
        if let Some(v) = raw.sidecar_fallback_enabled.as_deref() {
            match v {
                "true" => cfg.sidecar_fallback_enabled = true,
                "false" => cfg.sidecar_fallback_enabled = false,
                _ => {}
            }
        }
        if let Some(v) = raw
            .sidecar_fallback_min_interval_s
            .as_deref()
            .and_then(|s| s.parse::<u64>().ok())
        {
            if RANGE_SIDECAR_FALLBACK_MIN_INTERVAL_S.contains(&v) {
                cfg.sidecar_fallback_min_interval = Duration::from_secs(v);
            }
        }
        if let Some(v) = raw
            .sidecar_fallback_dormant_interval_s
            .as_deref()
            .and_then(|s| s.parse::<u64>().ok())
        {
            if RANGE_SIDECAR_FALLBACK_MIN_INTERVAL_S.contains(&v) {
                cfg.sidecar_fallback_dormant_interval = Duration::from_secs(v);
            }
        }
        if let Some(v) = raw
            .sidecar_fallback_archived_interval_s
            .as_deref()
            .and_then(|s| s.parse::<u64>().ok())
        {
            if RANGE_SIDECAR_FALLBACK_ARCHIVED_INTERVAL_S.contains(&v) {
                cfg.sidecar_fallback_archived_interval = Duration::from_secs(v);
            }
        }
        if let Some(v) = raw
            .sidecar_dormant_threshold_days
            .as_deref()
            .and_then(|s| s.parse::<u64>().ok())
        {
            if RANGE_SIDECAR_THRESHOLD_DAYS.contains(&v) {
                cfg.sidecar_dormant_threshold_days = v;
            }
        }
        if let Some(v) = raw
            .sidecar_archived_threshold_days
            .as_deref()
            .and_then(|s| s.parse::<u64>().ok())
        {
            if RANGE_SIDECAR_THRESHOLD_DAYS.contains(&v) {
                cfg.sidecar_archived_threshold_days = v;
            }
        }
        if let Some(v) = raw
            .sidecar_fallback_max_per_hour
            .as_deref()
            .and_then(|s| s.parse::<u64>().ok())
        {
            if RANGE_SIDECAR_FALLBACK_MAX_PER_HOUR.contains(&v) {
                cfg.sidecar_fallback_max_per_hour = v;
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
/// Minimum 1 minute (so a stuck-on-error source can't spam the sidecar
/// every loop), up to 24 hours (effectively "fallback once a day per
/// source"). Default for active channels is 1 hour.
pub const RANGE_SIDECAR_FALLBACK_MIN_INTERVAL_S: std::ops::RangeInclusive<u64> = 60..=86_400;
/// Archived interval extends up to 1 week — a never-publishing channel
/// can tolerate a longer gap on the riskier transport.
pub const RANGE_SIDECAR_FALLBACK_ARCHIVED_INTERVAL_S: std::ops::RangeInclusive<u64> = 60..=604_800;
/// Recency bucket threshold (days). 1..3650 covers "1 day" up to
/// "10 years", which is plenty of headroom.
pub const RANGE_SIDECAR_THRESHOLD_DAYS: std::ops::RangeInclusive<u64> = 1..=3_650;
/// `0` means unlimited; otherwise cap at 10 000/hour (well above any
/// home-server need, but enough headroom for an operator scaling to
/// thousands of sources who has explicitly raised it).
pub const RANGE_SIDECAR_FALLBACK_MAX_PER_HOUR: std::ops::RangeInclusive<u64> = 0..=10_000;

/// Public entry point: spawn the refresher onto the current runtime.
/// Hands back a `JoinHandle` purely for testability; production code
/// just discards it.
pub fn spawn(pool: SqlitePool) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        run(pool).await;
    })
}

/// Default `User-Agent` for the feed refresher's HTTP client. A generic
/// recent Chrome-on-Linux string is used because YouTube's RSS edge
/// intermittently responds with 404/500 to bot-style identifiers
/// (`hometube/0.1` etc.) even for channels whose Atom feed is
/// otherwise served fine.
///
/// The string is pinned to a specific Chrome major and will eventually
/// look outdated to YouTube; `HOMETUBE_RSS_USER_AGENT` (read by
/// `rss_user_agent`) lets an operator override it without a code
/// change when that day comes. Keep this in step with whatever
/// version a current desktop Chrome reports for its UA — a few
/// majors stale is fine, decades stale is not.
const DEFAULT_RSS_USER_AGENT: &str = "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 \
     (KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36";

/// Resolve the User-Agent for the feed refresher's HTTP client.
///
/// Reads `HOMETUBE_RSS_USER_AGENT` from the process environment and
/// falls back to [`DEFAULT_RSS_USER_AGENT`]. The env var is read once
/// at refresher startup (this function is called from `run` before
/// the long-lived `Client` is built), so changes require a restart —
/// matching how the rest of the service handles config that isn't
/// stored in `app_config`.
fn rss_user_agent() -> String {
    std::env::var("HOMETUBE_RSS_USER_AGENT")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| DEFAULT_RSS_USER_AGENT.to_string())
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
        .user_agent(rss_user_agent())
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

        // Construct the sidecar client once per tick so config changes
        // to `discovery_sidecar_url` are picked up without a restart.
        // Skipped (= no fallback this tick) if construction fails or
        // the fallback is disabled — the latter being the explicit
        // operator kill switch.
        let yt_for_fallback = if cfg.sidecar_fallback_enabled {
            match YoutubeClient::from_db(&pool).await {
                Ok(c) => Some(c),
                Err(err) => {
                    warn!(%err, "feed refresher: YoutubeClient construction failed; fallback disabled this tick");
                    None
                }
            }
        } else {
            None
        };

        // Read the aggregate-cap count once per tick. Each spawned
        // task gets a snapshot rather than running its own COUNT(*)
        // query — at default max_inflight=4 that's 4 identical
        // queries per tick we don't need to run.
        //
        // The snapshot is stale within the tick: a fallback dispatched
        // at the start doesn't get counted by tasks dispatched
        // milliseconds later. Worst case the aggregate cap can be
        // exceeded by up to `(batch_size - 1)` calls in a single tick
        // — but the per-source cap (which *is* always fresh, via the
        // reservation write) caps individual sources, and the next
        // tick will read the updated count and start denying.
        // Acceptable slack given the aggregate cap exists to prevent
        // sustained-outage storms, not single-tick precision.
        let fallbacks_this_hour: u64 =
            if cfg.sidecar_fallback_max_per_hour > 0 && yt_for_fallback.is_some() {
                feed_cache::sidecar_fallbacks_in_last_hour(&pool, unix_now())
                    .await
                    .unwrap_or(0) as u64
            } else {
                0
            };

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
            let yt = yt_for_fallback.clone();

            inflight.push(tokio::spawn(async move {
                if let Err(err) = poll_one(
                    &pool,
                    &http,
                    &base,
                    yt.as_ref(),
                    fallbacks_this_hour,
                    &source,
                    cfg,
                )
                .await
                {
                    warn!(
                        channel_id = %source.channel_id,
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

/// Drive one channel through poll + persist. Errors here are logged
/// but swallowed by the outer caller; per-channel failures are also
/// recorded in `channel_sync_state.rss_last_error` for diagnostics.
///
/// Tries RSS first (cheapest, lowest anti-bot risk). On RSS error,
/// falls back to the youtubei.js sidecar if the per-source and
/// aggregate rate caps permit.
#[tracing::instrument(
    name = "feed.poll",
    skip_all,
    fields(channel_id = %source.channel_id),
)]
async fn poll_one(
    pool: &SqlitePool,
    http: &Client,
    base: &str,
    yt: Option<&YoutubeClient>,
    fallbacks_this_hour: u64,
    source: &DueSource,
    cfg: RefresherConfig,
) -> crate::error::AppResult<()> {
    let now = unix_now();

    let result = youtube_rss::poll_channel(
        http,
        base,
        &source.channel_id,
        source.rss_etag.as_deref(),
        source.rss_last_modified.as_deref(),
    )
    .await;

    match result {
        Ok(PollOutcome::NotModified) => {
            let next = now + jittered_interval(cfg.channel_interval);
            feed_cache::record_poll_success(
                pool,
                feed_cache::PollSuccess {
                    channel_id: &source.channel_id,
                    title: None,
                    etag: source.rss_etag.as_deref(),
                    last_modified: source.rss_last_modified.as_deref(),
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
            feed_cache::upsert_channel_videos_from_rss(pool, &source.channel_id, &items, now)
                .await?;
            let next = now + jittered_interval(cfg.channel_interval);
            feed_cache::record_poll_success(
                pool,
                feed_cache::PollSuccess {
                    channel_id: &source.channel_id,
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
        Err(rss_err) => {
            let rss_msg = rss_err.to_string();

            // Try the sidecar fallback before recording a hard
            // failure. Skipped when the kill switch is off, the
            // client failed to construct earlier, or either rate cap
            // would be exceeded. The per-source cap is adaptive: an
            // archived channel that hasn't published in months tolerates
            // a longer gap on the riskier transport, so we look up its
            // most-recent upload to pick the right bucket.
            if let Some(client) = yt {
                let effective_interval =
                    effective_sidecar_min_interval(pool, &source.channel_id, cfg, now).await;
                if fallback_caps_permit(source, effective_interval, cfg, fallbacks_this_hour, now) {
                    return run_sidecar_fallback(pool, client, source, cfg, now, Some(&rss_msg))
                        .await;
                }
            }

            let next =
                now + backoff_for_attempt(source.rss_consecutive_errors + 1, cfg.channel_interval);
            feed_cache::record_poll_failure(pool, &source.channel_id, &rss_msg, next, now).await?;
            warn!(error = %rss_msg, "source poll failed");
        }
    }
    Ok(())
}

/// Both rate caps must permit a fallback before we dispatch one:
///
/// - **Per-source cap**: at least `effective_min_interval` seconds must
///   have elapsed since this source's last fallback. The effective
///   interval is bucketed by recency — see
///   [`effective_sidecar_min_interval`].
/// - **Aggregate cap**: `fallbacks_this_hour` (captured at the start
///   of the outer loop tick) must be below
///   `cfg.sidecar_fallback_max_per_hour`. A configured value of `0`
///   disables this cap (per-source still applies).
fn fallback_caps_permit(
    source: &DueSource,
    effective_min_interval_s: i64,
    cfg: RefresherConfig,
    fallbacks_this_hour: u64,
    now: i64,
) -> bool {
    if let Some(last) = source.last_sidecar_fallback_at {
        if now - last < effective_min_interval_s {
            debug!(
                channel_id = %source.channel_id,
                elapsed = now - last,
                min_interval = effective_min_interval_s,
                "skip sidecar fallback: per-source cap",
            );
            return false;
        }
    }

    if cfg.sidecar_fallback_max_per_hour > 0
        && fallbacks_this_hour >= cfg.sidecar_fallback_max_per_hour
    {
        debug!(
            channel_id = %source.channel_id,
            count = fallbacks_this_hour,
            cap = cfg.sidecar_fallback_max_per_hour,
            "skip sidecar fallback: aggregate cap",
        );
        return false;
    }
    true
}

/// Compute the effective per-source min interval for the sidecar
/// fallback based on the channel's most-recent upload. Channels with
/// uploads in the last `sidecar_dormant_threshold_days` keep the
/// "active" interval; channels dormant 30-90 days move to the
/// "dormant" interval (default 6h); channels archived for >90 days
/// move to the "archived" interval (default 24h).
///
/// One indexed query per RSS-failure event. The most-recent-upload
/// lookup hits `idx_channel_videos_channel_published`.
async fn effective_sidecar_min_interval(
    pool: &SqlitePool,
    channel_id: &str,
    cfg: RefresherConfig,
    now: i64,
) -> i64 {
    let max_published: Option<i64> = sqlx::query_scalar(
        "SELECT MAX(published_at) FROM channel_videos \
          WHERE channel_id = ? AND is_deleted = 0",
    )
    .bind(channel_id)
    .fetch_optional(pool)
    .await
    .ok()
    .flatten()
    .flatten();

    let days_since = max_published
        .map(|ts| (now - ts).max(0) / 86_400)
        .unwrap_or(i64::MAX);

    let dormant_days = cfg.sidecar_dormant_threshold_days as i64;
    let archived_days = cfg.sidecar_archived_threshold_days as i64;

    let bucket = if days_since <= dormant_days {
        cfg.sidecar_fallback_min_interval
    } else if days_since <= archived_days {
        cfg.sidecar_fallback_dormant_interval
    } else {
        cfg.sidecar_fallback_archived_interval
    };
    bucket.as_secs() as i64
}

/// Dispatch the sidecar fallback for one channel. Reservation is
/// written *before* the network call so a concurrent loop iteration or
/// a fast restart sees the per-source cap in effect even if the call
/// itself takes a while.
async fn run_sidecar_fallback(
    pool: &SqlitePool,
    yt: &YoutubeClient,
    source: &DueSource,
    cfg: RefresherConfig,
    now: i64,
    rss_err: Option<&str>,
) -> crate::error::AppResult<()> {
    feed_cache::record_sidecar_fallback_dispatched(pool, &source.channel_id, now).await?;

    let outcome = yt
        .refresher_list_channel_videos(&source.channel_id, SIDECAR_FALLBACK_MAX_ITEMS)
        .await;

    match outcome {
        SidecarRefresherOutcome::Items(items) => {
            let rows: Vec<ItemRow> = items
                .iter()
                .map(|it| sidecar_item_to_row(it, now))
                .collect();
            feed_cache::upsert_channel_videos_from_sidecar(pool, &source.channel_id, &rows, now)
                .await?;
            let next = now + jittered_interval(cfg.channel_interval);
            feed_cache::record_poll_success(
                pool,
                feed_cache::PollSuccess {
                    channel_id: &source.channel_id,
                    // The sidecar response doesn't carry the source
                    // title (it lives in a separate sidecar endpoint),
                    // so we leave the existing title untouched.
                    title: None,
                    etag: None,
                    last_modified: None,
                    next_poll_at: next,
                    now,
                },
            )
            .await?;
            info!(
                channel_id = %source.channel_id,
                count = rows.len(),
                "sidecar fallback succeeded",
            );
        }
        SidecarRefresherOutcome::NotFound => {
            let attempt = source.rss_consecutive_errors + 1;
            if attempt >= SIDECAR_NOTFOUND_SHELVE_THRESHOLD {
                feed_cache::record_source_dead(
                    pool,
                    &source.channel_id,
                    "sidecar reports source not found",
                    now + DEAD_SOURCE_DEFER_SECS,
                    now,
                )
                .await?;
                info!(
                    channel_id = %source.channel_id,
                    attempt,
                    "sidecar fallback classified source as dead; deferring 1 year",
                );
            } else {
                let next = now + backoff_for_attempt(attempt, cfg.channel_interval);
                let msg = format!(
                    "sidecar reports source not found ({}/{})",
                    attempt, SIDECAR_NOTFOUND_SHELVE_THRESHOLD
                );
                feed_cache::record_poll_failure(pool, &source.channel_id, &msg, next, now).await?;
                info!(
                    channel_id = %source.channel_id,
                    attempt,
                    threshold = SIDECAR_NOTFOUND_SHELVE_THRESHOLD,
                    "sidecar fallback returned NotFound; backing off, will retry",
                );
            }
        }
        SidecarRefresherOutcome::Error(sidecar_err) => {
            let combined = match rss_err {
                Some(r) => format!("rss: {r}; sidecar: {sidecar_err}"),
                None => format!("sidecar: {sidecar_err}"),
            };
            let next =
                now + backoff_for_attempt(source.rss_consecutive_errors + 1, cfg.channel_interval);
            feed_cache::record_poll_failure(pool, &source.channel_id, &combined, next, now).await?;
            warn!(error = %combined, "sidecar fallback errored; recording soft failure");
        }
    }
    Ok(())
}

/// Adapter from the sidecar's `ChannelVideoItem` shape to the
/// `feed_cache::ItemRow` shape `replace_source_items` consumes.
///
/// Sidecar `published_at` is a human-readable relative string
/// ("3 days ago") rather than an ISO 8601 timestamp, because
/// YouTube's InnerTube responses describe upload time that way and
/// youtubei.js passes it through. We keep the raw string in
/// `published_raw` (the frontend renders it directly) and best-effort
/// convert it to an approximate unix timestamp for the numeric
/// `published_at` so the "new videos" feed orders sidecar items
/// alongside RSS-sourced items with real timestamps. If the string
/// can't be parsed we leave `published_at = None`; the
/// `feed_for_child` query then falls back to `fetched_at` via
/// `COALESCE(published_at, fetched_at) DESC`.
fn sidecar_item_to_row(item: &SidecarChannelVideoItem, now: i64) -> ItemRow {
    // Pick a thumbnail by descending quality, mirroring the heuristic
    // used elsewhere (routes/feed.rs `pick_thumbnail` and similar).
    let thumbnail_url = ["maxres", "high", "standard", "medium", "default"]
        .into_iter()
        .find_map(|k| item.thumbnails.get(k))
        .map(|t| t.url.clone());
    let published_at = item.published_at.as_deref().and_then(|s| {
        let parsed = parse_relative_to_unix(s, now);
        if parsed.is_none() && !s.trim().is_empty() {
            // Surface format drift (locale leaks, new InnerTube shapes,
            // comma-grouped numbers, etc.) so we can extend the parser
            // before too many rows accumulate without a timestamp.
            tracing::debug!(raw = %s, "sidecar relative-time string not parseable");
        }
        parsed
    });
    ItemRow {
        video_id: item.video_id.clone(),
        title: item.title.clone(),
        channel_id: item.channel_id.clone(),
        channel_title: item.channel_title.clone(),
        thumbnail_url,
        published_at,
        published_raw: item.published_at.clone(),
    }
}

/// Best-effort parser for YouTube/InnerTube relative time strings
/// like "3 days ago", "1 hour ago", "Streamed 2 weeks ago", or
/// "Premiered 5 months ago". Returns an approximate unix timestamp
/// (`now - offset`) or `None` if the string doesn't match the
/// expected shape.
///
/// Assumes English-language InnerTube output (youtubei.js's default).
/// If a locale ever leaks through, items become unparseable and fall
/// back to `fetched_at` via the `feed_for_child` ORDER BY rather
/// than producing nonsense timestamps.
///
/// Month and year are approximated as 30 and 365 days respectively;
/// this is good enough for "new videos" ordering, where the relative
/// order between sidecar items and RSS items is what matters, not
/// sub-day precision.
pub(crate) fn parse_relative_to_unix(raw: &str, now: i64) -> Option<i64> {
    // Lowercase and strip known prefixes ("Streamed", "Streamed live",
    // "Premiered") and the trailing "ago".
    let lower = raw.trim().to_ascii_lowercase();
    let body = lower
        .strip_prefix("streamed live ")
        .or_else(|| lower.strip_prefix("streamed "))
        .or_else(|| lower.strip_prefix("premiered "))
        .unwrap_or(&lower);
    let body = body.strip_suffix(" ago").unwrap_or(body).trim();

    // Common no-offset / fixed-offset shorthands.
    match body {
        "just now" | "moments" | "a moment" => return Some(now),
        "yesterday" => return Some(now.saturating_sub(86_400)),
        _ => {}
    }

    // Tokenise. Three accepted shapes:
    //   1. Long form, 2 tokens:  "3 days", "an hour"
    //   2. Long form, 3 tokens:  "a few seconds", "a couple of hours"
    //   3. Compact form, 1 token: "3d", "2w", "10mo", "1y" — what
    //      current InnerTube / youtubei.js emits.
    let toks: Vec<&str> = body.split_whitespace().collect();
    let (count_tok, unit_tok): (&str, &str) = match toks.as_slice() {
        [single] => split_compact_token(single)?,
        [c, u] => (*c, *u),
        ["a", "few", u] => ("few", *u),
        ["a", "couple", u] | ["a", "couple", "of", u] => ("couple", *u),
        _ => return None,
    };
    // Parse as unsigned so "-3 days ago" can't yield a future
    // timestamp that would pin the item to the top of the feed.
    // Also reject thousands separators / decimals to keep semantics
    // unambiguous; we'd rather log and fall back than guess.
    let count: i64 = match count_tok {
        "a" | "an" => 1,
        "few" => 3,
        "couple" => 2,
        n => n.parse::<u32>().ok()? as i64,
    };
    let unit_secs: i64 = match unit_tok.trim_end_matches('s') {
        "second" => 1,
        "minute" => 60,
        "hour" => 3_600,
        "day" => 86_400,
        "week" => 7 * 86_400,
        "month" => 30 * 86_400,
        "year" => 365 * 86_400,
        _ => return None,
    };
    // Saturating multiply, then refuse to emit a negative-far-past
    // timestamp: if the offset exceeds `now`, the input is absurd —
    // signal "unparseable" so callers fall back to fetched_at instead
    // of pinning the row to ~1970.
    let offset = count.saturating_mul(unit_secs);
    if offset > now {
        return None;
    }
    Some(now - offset)
}

/// Split a glued compact relative-time token like `"3d"`, `"10mo"`,
/// `"1y"` into `(count, long_unit)` so the long-form match arm in
/// `parse_relative_to_unix` can resolve the unit without growing
/// further. Returns `None` if `token` is not in `<digits><suffix>`
/// shape with a recognised suffix.
///
/// Supported suffixes (matching YouTube/InnerTube output):
/// `s` second, `m` minute, `h` hour, `d` day, `w` week, `mo` month,
/// `y` year. The two-letter `mo` is deliberately distinct from `m`
/// (YouTube uses exactly this disambiguation; we follow it).
fn split_compact_token(token: &str) -> Option<(&str, &'static str)> {
    // Find the boundary between the leading digit run and the unit
    // suffix. Using char_indices keeps this byte-safe even though
    // all expected inputs are ASCII.
    let digits_end = token
        .char_indices()
        .find_map(|(i, c)| if c.is_ascii_digit() { None } else { Some(i) })
        .unwrap_or(token.len());
    if digits_end == 0 || digits_end == token.len() {
        return None;
    }
    let (count, suffix) = token.split_at(digits_end);
    // Map back to the long forms so the existing `match` arms handle
    // the unit lookup without duplicating the seconds-per-unit table.
    let long_unit = match suffix {
        "s" => "seconds",
        "m" => "minutes",
        "h" => "hours",
        "d" => "days",
        "w" => "weeks",
        "mo" => "months",
        "y" => "years",
        _ => return None,
    };
    Some((count, long_unit))
}

#[cfg(test)]
mod relative_parser_tests {
    use super::parse_relative_to_unix;

    const NOW: i64 = 1_700_000_000;

    #[test]
    fn parses_common_units() {
        assert_eq!(
            parse_relative_to_unix("30 seconds ago", NOW),
            Some(NOW - 30)
        );
        assert_eq!(
            parse_relative_to_unix("5 minutes ago", NOW),
            Some(NOW - 300)
        );
        assert_eq!(parse_relative_to_unix("2 hours ago", NOW), Some(NOW - 7200));
        assert_eq!(
            parse_relative_to_unix("3 days ago", NOW),
            Some(NOW - 3 * 86_400)
        );
        assert_eq!(
            parse_relative_to_unix("1 week ago", NOW),
            Some(NOW - 7 * 86_400)
        );
        assert_eq!(
            parse_relative_to_unix("2 months ago", NOW),
            Some(NOW - 60 * 86_400)
        );
        assert_eq!(
            parse_relative_to_unix("1 year ago", NOW),
            Some(NOW - 365 * 86_400)
        );
    }

    #[test]
    fn parses_singular_and_articles() {
        assert_eq!(parse_relative_to_unix("1 day ago", NOW), Some(NOW - 86_400));
        assert_eq!(parse_relative_to_unix("a day ago", NOW), Some(NOW - 86_400));
        assert_eq!(
            parse_relative_to_unix("an hour ago", NOW),
            Some(NOW - 3_600)
        );
    }

    #[test]
    fn strips_streamed_and_premiered_prefix() {
        assert_eq!(
            parse_relative_to_unix("Streamed 2 weeks ago", NOW),
            Some(NOW - 14 * 86_400)
        );
        assert_eq!(
            parse_relative_to_unix("Streamed live 4 hours ago", NOW),
            Some(NOW - 4 * 3_600)
        );
        assert_eq!(
            parse_relative_to_unix("Premiered 5 months ago", NOW),
            Some(NOW - 150 * 86_400)
        );
    }

    #[test]
    fn parses_few_and_couple() {
        assert_eq!(
            parse_relative_to_unix("a few seconds ago", NOW),
            Some(NOW - 3)
        );
        assert_eq!(
            parse_relative_to_unix("a couple hours ago", NOW),
            Some(NOW - 2 * 3_600)
        );
        assert_eq!(
            parse_relative_to_unix("a couple of days ago", NOW),
            Some(NOW - 2 * 86_400)
        );
    }

    #[test]
    fn case_and_whitespace_insensitive() {
        assert_eq!(
            parse_relative_to_unix("  3 DAYS AGO  ", NOW),
            Some(NOW - 3 * 86_400)
        );
    }

    #[test]
    fn parses_fixed_offset_shorthands() {
        assert_eq!(parse_relative_to_unix("just now", NOW), Some(NOW));
        assert_eq!(parse_relative_to_unix("yesterday", NOW), Some(NOW - 86_400));
    }

    #[test]
    fn unparseable_returns_none() {
        assert_eq!(parse_relative_to_unix("", NOW), None);
        assert_eq!(parse_relative_to_unix("3 fortnights ago", NOW), None);
        assert_eq!(parse_relative_to_unix("2024-01-01", NOW), None);
        // Thousands separators / decimals are rejected (we'd rather
        // fall back to fetched_at than guess at the intent).
        assert_eq!(parse_relative_to_unix("1,234 days ago", NOW), None);
        assert_eq!(parse_relative_to_unix("1.5 hours ago", NOW), None);
    }

    #[test]
    fn rejects_negative_counts() {
        // Negative counts must not produce a future timestamp.
        assert_eq!(parse_relative_to_unix("-3 days ago", NOW), None);
    }

    #[test]
    fn extreme_counts_return_none_instead_of_negative_timestamp() {
        // u32::MAX years is wildly out of range; we'd rather return
        // None (callers fall back to fetched_at) than emit a deeply
        // negative timestamp that would pin the row to the far past.
        assert_eq!(parse_relative_to_unix("4294967295 years ago", NOW), None);
    }

    /// Current InnerTube / youtubei.js output uses compact glued
    /// tokens (`2d ago`, `3mo ago`, `1y ago`, …). Each of these used
    /// to silently fall through to `None`, leaving sidecar-sourced
    /// rows ordered by `fetched_at` (effectively "now") regardless
    /// of how old the upload actually was.
    #[test]
    fn parses_compact_youtube_abbreviations() {
        assert_eq!(parse_relative_to_unix("30s ago", NOW), Some(NOW - 30));
        assert_eq!(parse_relative_to_unix("5m ago", NOW), Some(NOW - 5 * 60));
        assert_eq!(parse_relative_to_unix("2h ago", NOW), Some(NOW - 2 * 3_600));
        assert_eq!(
            parse_relative_to_unix("2d ago", NOW),
            Some(NOW - 2 * 86_400)
        );
        assert_eq!(
            parse_relative_to_unix("9d ago", NOW),
            Some(NOW - 9 * 86_400)
        );
        assert_eq!(
            parse_relative_to_unix("3w ago", NOW),
            Some(NOW - 3 * 7 * 86_400)
        );
        assert_eq!(
            parse_relative_to_unix("4mo ago", NOW),
            Some(NOW - 4 * 30 * 86_400)
        );
        assert_eq!(
            parse_relative_to_unix("11mo ago", NOW),
            Some(NOW - 11 * 30 * 86_400)
        );
        assert_eq!(
            parse_relative_to_unix("1y ago", NOW),
            Some(NOW - 365 * 86_400)
        );
        assert_eq!(
            parse_relative_to_unix("13y ago", NOW),
            Some(NOW - 13 * 365 * 86_400)
        );
    }

    /// The two-letter `mo` must not be confused with the single-letter
    /// `m` (minute). YouTube specifically uses this disambiguation,
    /// so getting it wrong would silently mis-date every monthly
    /// upload by a factor of 43,200.
    #[test]
    fn compact_m_vs_mo_distinguished() {
        assert_eq!(parse_relative_to_unix("3m ago", NOW), Some(NOW - 3 * 60));
        assert_eq!(
            parse_relative_to_unix("3mo ago", NOW),
            Some(NOW - 3 * 30 * 86_400)
        );
    }

    /// Compact tokens with no recognised suffix, or no leading digits,
    /// must still fail closed.
    #[test]
    fn compact_unknown_suffix_returns_none() {
        assert_eq!(parse_relative_to_unix("3z ago", NOW), None);
        assert_eq!(parse_relative_to_unix("d ago", NOW), None);
        assert_eq!(parse_relative_to_unix("mo ago", NOW), None);
    }

    /// Prefix stripping ("Streamed", "Premiered") still applies to
    /// compact-form bodies — InnerTube emits e.g. "Streamed 2d ago"
    /// for past livestreams.
    #[test]
    fn compact_with_streamed_premiered_prefix() {
        assert_eq!(
            parse_relative_to_unix("Streamed 2d ago", NOW),
            Some(NOW - 2 * 86_400)
        );
        assert_eq!(
            parse_relative_to_unix("Premiered 3mo ago", NOW),
            Some(NOW - 3 * 30 * 86_400)
        );
    }
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

    // -----------------------------------------------------------------
    // effective_sidecar_min_interval — three recency buckets driven
    // by the channel's most-recent upload.
    // -----------------------------------------------------------------

    use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
    use std::str::FromStr;

    async fn setup_pool() -> sqlx::SqlitePool {
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

    async fn seed_channel_with_max_published(
        pool: &sqlx::SqlitePool,
        channel_id: &str,
        published_at: Option<i64>,
    ) {
        sqlx::query(
            "INSERT INTO channels (channel_id, backfill_status, backfill_next_at, rss_next_poll_at) \
             VALUES (?, 'pending', 0, 0)",
        )
        .bind(channel_id)
        .execute(pool)
        .await
        .unwrap();
        if let Some(ts) = published_at {
            let vid = format!("v-{channel_id}");
            crate::models::video::upsert_stub(pool, &vid).await.unwrap();
            sqlx::query(
                "INSERT INTO channel_videos \
                    (channel_id, video_id, published_at, \
                     first_seen_at, last_seen_at, source) \
                 VALUES (?, ?, ?, 1, 1, 'rss')",
            )
            .bind(channel_id)
            .bind(&vid)
            .bind(ts)
            .execute(pool)
            .await
            .unwrap();
        }
    }

    #[tokio::test]
    async fn effective_sidecar_min_interval_uses_active_bucket_for_recent_upload() {
        let pool = setup_pool().await;
        let now = 1_700_000_000;
        // Uploaded today.
        seed_channel_with_max_published(&pool, "UCactive", Some(now)).await;
        let cfg = RefresherConfig::default();
        let interval = effective_sidecar_min_interval(&pool, "UCactive", cfg, now).await;
        assert_eq!(interval, cfg.sidecar_fallback_min_interval.as_secs() as i64);
    }

    #[tokio::test]
    async fn effective_sidecar_min_interval_uses_dormant_bucket_for_30_to_90_days() {
        let pool = setup_pool().await;
        let now = 1_700_000_000;
        // 60 days ago — past the dormant threshold but inside the
        // archived threshold.
        let sixty_days_ago = now - 60 * 86_400;
        seed_channel_with_max_published(&pool, "UCdormant", Some(sixty_days_ago)).await;
        let cfg = RefresherConfig::default();
        let interval = effective_sidecar_min_interval(&pool, "UCdormant", cfg, now).await;
        assert_eq!(
            interval,
            cfg.sidecar_fallback_dormant_interval.as_secs() as i64
        );
    }

    #[tokio::test]
    async fn effective_sidecar_min_interval_uses_archived_bucket_for_old_uploads() {
        let pool = setup_pool().await;
        let now = 1_700_000_000;
        // 200 days ago — past both thresholds.
        let two_hundred_days_ago = now - 200 * 86_400;
        seed_channel_with_max_published(&pool, "UCold", Some(two_hundred_days_ago)).await;
        let cfg = RefresherConfig::default();
        let interval = effective_sidecar_min_interval(&pool, "UCold", cfg, now).await;
        assert_eq!(
            interval,
            cfg.sidecar_fallback_archived_interval.as_secs() as i64
        );
    }

    #[tokio::test]
    async fn effective_sidecar_min_interval_uses_archived_bucket_when_no_videos() {
        let pool = setup_pool().await;
        let now = 1_700_000_000;
        // No channel_videos rows at all — most_published is NULL,
        // days_since = i64::MAX, falls into the archived bucket.
        seed_channel_with_max_published(&pool, "UCempty", None).await;
        let cfg = RefresherConfig::default();
        let interval = effective_sidecar_min_interval(&pool, "UCempty", cfg, now).await;
        assert_eq!(
            interval,
            cfg.sidecar_fallback_archived_interval.as_secs() as i64
        );
    }
}
