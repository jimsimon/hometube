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

use crate::services::feed_cache::{self, DueSource, ItemRow, KIND_CHANNEL, KIND_PLAYLIST};
use crate::services::youtube::{
    PlaylistItem as SidecarPlaylistItem, SidecarRefresherOutcome, YoutubeClient,
};
use crate::services::youtube_rss::{self, PollOutcome};

/// How many items the sidecar fallback asks for per source. Matches
/// `PER_SOURCE_CAP` (the storage cap) and the RSS feed's ~15-item
/// natural ceiling so the two paths produce comparably-sized writes.
const SIDECAR_FALLBACK_MAX_ITEMS: u32 = 15;

/// When the sidecar confirms a source is dead (`NotFound`), push its
/// `next_poll_at` this far into the future. The row is preserved so a
/// future reactivation (channel restored, or the operator manually
/// resets `next_poll_at`) can pick it back up.
const DEAD_SOURCE_DEFER_SECS: i64 = 365 * 24 * 60 * 60;

/// How many consecutive sidecar `NotFound` responses we require before
/// shelving a source as dead. A single 404 isn't enough because
/// playlists routinely flip between public and private (so privacy
/// toggles would otherwise permanently shelve them), and channels can
/// briefly 404 during YouTube-side glitches. Each preliminary 404
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

/// Per-source rate cap on sidecar fallbacks, in seconds. The refresher
/// will not issue a second fallback for the same source until at
/// least this long has passed since the last one. Prevents a multi-
/// hour RSS outage from generating N sidecar calls per hour where N
/// is the source count.
pub const DEFAULT_SIDECAR_FALLBACK_MIN_INTERVAL_S: u64 = 60 * 60;

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
pub const KEY_SIDECAR_FALLBACK_MIN_INTERVAL_S: &str =
    "feed_refresher_sidecar_fallback_min_interval_s";
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
    /// See [`DEFAULT_SIDECAR_FALLBACK_MIN_INTERVAL_S`].
    pub sidecar_fallback_min_interval: Duration,
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
             WHERE key IN (?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(KEY_DISPATCH_DELAY_MS)
        .bind(KEY_MAX_INFLIGHT)
        .bind(KEY_BATCH_SIZE)
        .bind(KEY_IDLE_TICK_S)
        .bind(KEY_CHANNEL_INTERVAL_S)
        .bind(KEY_SIDECAR_FALLBACK_ENABLED)
        .bind(KEY_SIDECAR_FALLBACK_MIN_INTERVAL_S)
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
/// source"). Default is 1 hour.
pub const RANGE_SIDECAR_FALLBACK_MIN_INTERVAL_S: std::ops::RangeInclusive<u64> = 60..=86_400;
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
///
/// The poll strategy depends on the source kind:
///
/// - **Channel sources** try RSS first (cheapest, lowest anti-bot
///   risk). On RSS error, fall back to the youtubei.js sidecar if the
///   per-source and aggregate rate caps permit.
/// - **Playlist sources** have no RSS endpoint, so they go straight to
///   the sidecar fallback. The same rate caps apply. When the
///   sidecar fallback is disabled or unavailable, playlists are
///   shelved (1 year defer) — preserving the pre-fallback behaviour
///   so we don't accumulate `consecutive_errors` forever on a kind
///   we have no transport for.
#[tracing::instrument(
    name = "feed.poll",
    skip_all,
    fields(kind = %source.kind, source_id = %source.source_id),
)]
async fn poll_one(
    pool: &SqlitePool,
    http: &Client,
    base: &str,
    yt: Option<&YoutubeClient>,
    // Aggregate sidecar-fallback count captured at the start of the
    // outer loop tick. Per-task snapshot — see the comment in `run`.
    fallbacks_this_hour: u64,
    source: &DueSource,
    cfg: RefresherConfig,
) -> crate::error::AppResult<()> {
    let now = unix_now();

    if source.kind == KIND_PLAYLIST {
        // No RSS transport for playlists, so the sidecar is the
        // only path. Three sub-cases, each handled differently:
        match yt {
            // (1) Sidecar available and rate caps permit → poll.
            Some(client) if fallback_caps_permit(source, cfg, fallbacks_this_hour, now) => {
                run_sidecar_fallback(pool, client, source, cfg, now, None).await?;
                return Ok(());
            }
            // (2) Sidecar available but caps temporarily deny.
            // This is a *transient* condition (per-source min interval
            // or aggregate per-hour cap), so reschedule the normal
            // interval and try again next cycle. Using
            // `record_poll_deferred` (not `record_poll_skipped`) so
            // we don't clobber `consecutive_errors` if the source had
            // a real failure history before being rate-capped.
            Some(_) => {
                let next = now + jittered_interval(cfg.channel_interval);
                feed_cache::record_poll_deferred(
                    pool,
                    &source.kind,
                    &source.source_id,
                    "playlist: sidecar fallback rate-capped",
                    next,
                    now,
                )
                .await?;
                return Ok(());
            }
            // (3) Sidecar disabled (kill switch) or client failed to
            // construct → no working transport at all → shelve for a
            // year so the row doesn't keep getting retried with
            // nothing to call. Flipping the kill switch back on, or
            // restarting the sidecar, will pick the row up again as
            // soon as `next_poll_at` is manually advanced (or after
            // the 1 year defer elapses).
            None => {
                let one_year_secs: i64 = 365 * 24 * 60 * 60;
                feed_cache::record_poll_skipped(
                    pool,
                    &source.kind,
                    &source.source_id,
                    "playlist: sidecar fallback unavailable",
                    now + one_year_secs,
                )
                .await?;
                return Ok(());
            }
        }
    }

    if source.kind != KIND_CHANNEL {
        // Unknown kind. Shelve it the same way old playlist handling
        // worked so the row doesn't poison the diagnostics page.
        let one_year_secs: i64 = 365 * 24 * 60 * 60;
        feed_cache::record_poll_skipped(
            pool,
            &source.kind,
            &source.source_id,
            "kind not supported (deferred)",
            now + one_year_secs,
        )
        .await?;
        return Ok(());
    }

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
        Err(rss_err) => {
            let rss_msg = rss_err.to_string();

            // Try the sidecar fallback before recording a hard
            // failure. Skipped when the kill switch is off, the
            // client failed to construct earlier, or either rate cap
            // would be exceeded.
            if let Some(client) = yt {
                if fallback_caps_permit(source, cfg, fallbacks_this_hour, now) {
                    return run_sidecar_fallback(pool, client, source, cfg, now, Some(&rss_msg))
                        .await;
                }
            }

            let next =
                now + backoff_for_attempt(source.consecutive_errors + 1, cfg.channel_interval);
            feed_cache::record_poll_failure(
                pool,
                &source.kind,
                &source.source_id,
                &rss_msg,
                next,
                now,
            )
            .await?;
            warn!(error = %rss_msg, "source poll failed");
        }
    }
    Ok(())
}

/// Both rate caps must permit a fallback before we dispatch one:
///
/// - **Per-source cap**: at least `cfg.sidecar_fallback_min_interval`
///   must have elapsed since this source's last fallback. Persistent,
///   read from `DueSource::last_sidecar_fallback_at`. A `None` value
///   means "never fallen back" → permitted.
/// - **Aggregate cap**: `fallbacks_this_hour` (captured at the start
///   of the outer loop tick) must be below
///   `cfg.sidecar_fallback_max_per_hour`. A configured value of `0`
///   disables this cap (per-source still applies).
///
/// Pure function — no DB I/O — because both inputs are captured at
/// the start of the tick. Cheaper than the previous design that
/// queried `feed_sources` once per spawned task.
fn fallback_caps_permit(
    source: &DueSource,
    cfg: RefresherConfig,
    fallbacks_this_hour: u64,
    now: i64,
) -> bool {
    let min_interval = cfg.sidecar_fallback_min_interval.as_secs() as i64;
    if let Some(last) = source.last_sidecar_fallback_at {
        if now - last < min_interval {
            debug!(
                kind = %source.kind,
                source_id = %source.source_id,
                elapsed = now - last,
                "skip sidecar fallback: per-source cap",
            );
            return false;
        }
    }

    if cfg.sidecar_fallback_max_per_hour > 0
        && fallbacks_this_hour >= cfg.sidecar_fallback_max_per_hour
    {
        debug!(
            kind = %source.kind,
            source_id = %source.source_id,
            count = fallbacks_this_hour,
            cap = cfg.sidecar_fallback_max_per_hour,
            "skip sidecar fallback: aggregate cap",
        );
        return false;
    }
    true
}

/// Dispatch the sidecar fallback for one source. Reservation is
/// written *before* the network call so a concurrent loop iteration or
/// a fast restart sees the per-source cap in effect even if the call
/// itself takes a while. Classifies the outcome into items / dead /
/// soft-error.
///
/// `rss_err` is the upstream RSS error message (if any) that triggered
/// the fallback. Used purely for diagnostics in the `last_error`
/// column when both transports fail.
async fn run_sidecar_fallback(
    pool: &SqlitePool,
    yt: &YoutubeClient,
    source: &DueSource,
    cfg: RefresherConfig,
    now: i64,
    rss_err: Option<&str>,
) -> crate::error::AppResult<()> {
    // Reserve the slot *before* the network call. The per-source
    // cap is fail-safe even under concurrent dispatch: a second tick
    // checking `last_sidecar_fallback_at` will see the timestamp
    // from this write and back off.
    //
    // Consequence operators should be aware of: a fallback that
    // *fails* (sidecar 5xx, network timeout) still consumes a
    // per-source and aggregate slot. This is deliberate — under
    // sustained sidecar misbehaviour we'd rather burn one cap slot
    // and back off than retry-storm — but it means the diagnostics
    // UI's "Last fallback" column will tick on a *dispatch*, not on
    // a *successful* fallback. To distinguish, cross-reference
    // `last_error`: a sidecar-induced error message means the
    // fallback was attempted but failed; absence means it succeeded.
    feed_cache::record_sidecar_fallback_dispatched(pool, &source.kind, &source.source_id, now)
        .await?;

    let outcome = match source.kind.as_str() {
        KIND_CHANNEL => {
            yt.refresher_list_channel_videos(&source.source_id, SIDECAR_FALLBACK_MAX_ITEMS)
                .await
        }
        KIND_PLAYLIST => {
            yt.refresher_list_playlist_items(&source.source_id, SIDECAR_FALLBACK_MAX_ITEMS)
                .await
        }
        unknown => {
            // Shouldn't happen — `poll_one`'s `KIND_PLAYLIST` /
            // `KIND_CHANNEL` / shelve-and-return guards already
            // filter unknown kinds before we get here — but be
            // defensive rather than panicking inside the refresher
            // if a new kind is introduced upstream of this match.
            SidecarRefresherOutcome::Error(format!("unsupported kind: {unknown}"))
        }
    };

    match outcome {
        SidecarRefresherOutcome::Items(items) => {
            let rows: Vec<ItemRow> = items.iter().map(sidecar_item_to_row).collect();
            persist_items(pool, &source.kind, &source.source_id, &rows, now).await?;
            let next = now + jittered_interval(cfg.channel_interval);
            feed_cache::record_poll_success(
                pool,
                feed_cache::PollSuccess {
                    kind: &source.kind,
                    source_id: &source.source_id,
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
                kind = %source.kind,
                source_id = %source.source_id,
                count = rows.len(),
                "sidecar fallback succeeded",
            );
        }
        SidecarRefresherOutcome::NotFound => {
            // Debounced shelve: a single 404 isn't enough — privacy
            // toggles on playlists and brief YouTube-side glitches on
            // channels would otherwise permanently shelve live rows.
            // Require `SIDECAR_NOTFOUND_SHELVE_THRESHOLD` consecutive
            // failures (including this one), counted via the existing
            // `consecutive_errors` column.
            let attempt = source.consecutive_errors + 1;
            if attempt >= SIDECAR_NOTFOUND_SHELVE_THRESHOLD {
                feed_cache::record_source_dead(
                    pool,
                    &source.kind,
                    &source.source_id,
                    "sidecar reports source not found",
                    now + DEAD_SOURCE_DEFER_SECS,
                    now,
                )
                .await?;
                info!(
                    kind = %source.kind,
                    source_id = %source.source_id,
                    attempt,
                    "sidecar fallback classified source as dead; deferring 1 year",
                );
            } else {
                // Bump `consecutive_errors` and apply normal backoff;
                // we'll re-check on the next eligible poll. The
                // diagnostic message includes the running count so an
                // operator can see "2/3 NotFound" in the UI.
                let next = now + backoff_for_attempt(attempt, cfg.channel_interval);
                let msg = format!(
                    "sidecar reports source not found ({}/{})",
                    attempt, SIDECAR_NOTFOUND_SHELVE_THRESHOLD
                );
                feed_cache::record_poll_failure(
                    pool,
                    &source.kind,
                    &source.source_id,
                    &msg,
                    next,
                    now,
                )
                .await?;
                info!(
                    kind = %source.kind,
                    source_id = %source.source_id,
                    attempt,
                    threshold = SIDECAR_NOTFOUND_SHELVE_THRESHOLD,
                    "sidecar fallback returned NotFound; backing off, will retry",
                );
            }
        }
        SidecarRefresherOutcome::Error(sidecar_err) => {
            // Soft-fail: don't classify the source. Combine the
            // upstream RSS error (if any) with the sidecar error so
            // diagnostics show the full picture.
            let combined = match rss_err {
                Some(r) => format!("rss: {r}; sidecar: {sidecar_err}"),
                None => format!("sidecar: {sidecar_err}"),
            };
            let next =
                now + backoff_for_attempt(source.consecutive_errors + 1, cfg.channel_interval);
            feed_cache::record_poll_failure(
                pool,
                &source.kind,
                &source.source_id,
                &combined,
                next,
                now,
            )
            .await?;
            warn!(error = %combined, "sidecar fallback errored; recording soft failure");
        }
    }
    Ok(())
}

/// Adapter from the sidecar's `PlaylistItem` shape to the
/// `feed_cache::ItemRow` shape `replace_source_items` consumes.
///
/// Critical mismatch: sidecar `published_at` is a human-readable
/// relative string ("3 days ago") rather than an ISO 8601 timestamp,
/// because YouTube's InnerTube responses describe upload time that
/// way and youtubei.js passes it through. We store the raw string in
/// `published_raw` (where the frontend already knows how to display
/// it) and leave the numeric `published_at` as `None`. The
/// `feed_for_child` ordering query falls back to `fetched_at DESC`
/// when `published_at` is null, which is the right behaviour for
/// fallback items — they all arrived in one batch and their relative
/// order doesn't carry useful signal.
fn sidecar_item_to_row(item: &SidecarPlaylistItem) -> ItemRow {
    // Pick a thumbnail by descending quality, mirroring the heuristic
    // used elsewhere (routes/feed.rs `pick_thumbnail` and similar).
    let thumbnail_url = ["maxres", "high", "standard", "medium", "default"]
        .into_iter()
        .find_map(|k| item.thumbnails.get(k))
        .map(|t| t.url.clone());
    ItemRow {
        video_id: item.video_id.clone(),
        title: item.title.clone(),
        channel_id: item.channel_id.clone(),
        channel_title: item.channel_title.clone(),
        thumbnail_url,
        published_at: None,
        published_raw: item.published_at.clone(),
    }
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
