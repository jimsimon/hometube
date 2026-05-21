//! Child home-page feed routes.
//!
//! Three read-only endpoints back the child UI:
//!
//! - `GET /api/feed/continue-watching` — recently-watched items from
//!   `watch_history`, filtered through access control to drop videos
//!   the parent has since revoked.
//! - `GET /api/feed/new-videos` — fresh uploads from each allowlisted
//!   channel + each allowlisted playlist, deduped + sorted.
//! - `GET /api/feed/up-next` — the next videos to play after the one
//!   currently on screen, given a `from=playlist:ID|channel:ID|video:ID`
//!   context. Drops blocked + access-denied items.

use std::collections::HashSet;

use axum::{
    extract::{Query, State},
    Json,
};
use rand::seq::SliceRandom;
use rand::{rngs::StdRng, SeedableRng};
use serde::{Deserialize, Serialize};

use crate::error::AppResult;
use crate::middleware::auth::CurrentAccount;
use crate::services::access::{
    can_child_view, child_allowlisted_channel_ids, child_allowlisted_playlist_ids,
};
use crate::services::feed_cache;
use crate::state::AppState;

/// Default limit for "continue watching".
const CONTINUE_LIMIT: i64 = 20;
/// A video is considered "finished" (and dropped from continue-watching)
/// once the saved position is within this many seconds of the end. Picks
/// up the typical 5–15s of outro/credits that most viewers don't sit
/// through but the player still records before pause/ended fires.
const CONTINUE_TAIL_SECONDS: i64 = 15;
/// …or once the saved position is at least this fraction of the
/// duration, whichever triggers first. Catches short videos where the
/// absolute tail threshold would be larger than the whole runtime.
const CONTINUE_COMPLETION_RATIO: f64 = 0.95;
/// Hard cap on the new-videos feed.
const NEW_VIDEOS_LIMIT: usize = 30;

#[derive(Debug, Serialize, sqlx::FromRow, Clone)]
pub struct ContinueWatchingItem {
    pub video_id: String,
    pub video_title: String,
    pub video_thumbnail_url: Option<String>,
    pub channel_title: Option<String>,
    pub duration_seconds: Option<i64>,
    pub progress_seconds: i64,
    pub last_watched_at: i64,
    /// Skipped from the JSON wire shape (the row component doesn't need
    /// it) but required server-side so the access check can recognise
    /// channel-allowlisted videos.
    #[serde(skip_serializing)]
    pub channel_id: Option<String>,
}

/// `GET /api/feed/continue-watching`.
pub async fn continue_watching(
    State(state): State<AppState>,
    current: CurrentAccount,
) -> AppResult<Json<Vec<ContinueWatchingItem>>> {
    let rows: Vec<ContinueWatchingItem> = sqlx::query_as(
        "SELECT video_id, video_title, video_thumbnail_url, channel_title, \
                duration_seconds, progress_seconds, last_watched_at, channel_id \
         FROM watch_history \
         WHERE child_account_id = ? \
         ORDER BY last_watched_at DESC \
         LIMIT ?",
    )
    .bind(current.id)
    .bind(CONTINUE_LIMIT)
    .fetch_all(&state.db)
    .await?;

    // Filter through access control + drop effectively-finished videos.
    // `watch_history.channel_id` is the key bit: when a video was
    // surfaced via an allowlisted channel, the per-row `channel_id`
    // lets `can_child_view` find the channel allowlist hit. Rows that
    // pre-date that column (NULL) fall back to a best-effort lookup
    // against `feed_source_items`, batched into a single query so we
    // don't add an N+1 to the existing per-row access check.
    let legacy_ids: Vec<&str> = rows
        .iter()
        .filter(|r| r.channel_id.is_none())
        .map(|r| r.video_id.as_str())
        .collect();
    let legacy_map = lookup_channel_ids_for_videos(&state.db, &legacy_ids)
        .await
        .unwrap_or_default();

    let mut filtered = Vec::with_capacity(rows.len());
    for row in rows {
        if is_effectively_finished(row.progress_seconds, row.duration_seconds) {
            continue;
        }
        let channel_id = row
            .channel_id
            .clone()
            .or_else(|| legacy_map.get(&row.video_id).cloned());
        if can_child_view(&state.db, current.id, &row.video_id, channel_id.as_deref(), &[])
            .await
            .unwrap_or(false)
        {
            filtered.push(row);
        }
    }
    Ok(Json(filtered))
}

/// Best-effort batched `video_id → channel_id` lookup for
/// `watch_history` rows that were written before the `channel_id`
/// column existed (migration 013). Reads from `feed_source_items`,
/// which the feed refresher keeps populated for every allowlisted
/// channel + playlist. Returns an empty map when there are no legacy
/// rows to resolve, so the caller never pays for the round-trip on a
/// freshly-migrated DB.
async fn lookup_channel_ids_for_videos(
    db: &sqlx::SqlitePool,
    video_ids: &[&str],
) -> AppResult<std::collections::HashMap<String, String>> {
    if video_ids.is_empty() {
        return Ok(std::collections::HashMap::new());
    }
    // sqlx doesn't support binding a slice to `IN (?)` directly with
    // SQLite, so build the placeholder list manually. Inputs are
    // video_ids we just read out of our own DB, so there's nothing to
    // sanitise — but we still parameterise rather than interpolate.
    let placeholders = std::iter::repeat("?")
        .take(video_ids.len())
        .collect::<Vec<_>>()
        .join(",");
    let sql = format!(
        "SELECT video_id, channel_id FROM feed_source_items \
         WHERE channel_id IS NOT NULL AND video_id IN ({placeholders})"
    );
    let mut q = sqlx::query_as::<_, (String, String)>(&sql);
    for id in video_ids {
        q = q.bind(*id);
    }
    let rows = q.fetch_all(db).await?;
    // A single video_id can have rows under multiple `(kind, source_id)`
    // pairs (e.g. surfaced via both a channel feed and a playlist).
    // First match wins; we don't try to pick a "best" one because
    // `can_child_view` only needs *some* channel_id that hits the
    // allowlist.
    let mut map = std::collections::HashMap::with_capacity(rows.len());
    for (video_id, channel_id) in rows {
        map.entry(video_id).or_insert(channel_id);
    }
    Ok(map)
}

/// Whether a saved (`progress_seconds`, `duration_seconds`) pair should
/// be treated as "done" for continue-watching purposes. Rows with no
/// known duration are never auto-finished — we can't tell where the
/// end is, so we let the user remove them by re-watching.
fn is_effectively_finished(progress_seconds: i64, duration_seconds: Option<i64>) -> bool {
    let Some(duration) = duration_seconds else {
        return false;
    };
    if duration <= 0 || progress_seconds <= 0 {
        return false;
    }
    if progress_seconds >= duration.saturating_sub(CONTINUE_TAIL_SECONDS) {
        return true;
    }
    (progress_seconds as f64) >= (duration as f64) * CONTINUE_COMPLETION_RATIO
}

#[derive(Debug, Serialize, Clone)]
pub struct NewVideoItem {
    pub video_id: String,
    pub title: String,
    pub channel_id: Option<String>,
    pub channel_title: Option<String>,
    pub thumbnail_url: Option<String>,
    pub published_at: Option<String>,
    pub source_kind: String, // "channel" or "playlist"
    pub source_id: String,
}

/// `GET /api/admin/feed-refresher/settings` — parent-only.
///
/// Returns the live (post-validation) refresher tunables alongside the
/// raw `app_config` strings. When `raw` differs from the effective
/// value, the UI can show a warning so an operator who wrote
/// out-of-range garbage directly via SQL can spot the discrepancy.
pub async fn admin_get_refresher_settings(
    State(state): State<AppState>,
) -> AppResult<Json<RefresherSettings>> {
    let (cfg, raw) =
        crate::services::feed_refresher::RefresherConfig::load_with_raw(&state.db).await;
    Ok(Json(RefresherSettings {
        dispatch_delay_ms: cfg.dispatch_delay.as_millis() as u64,
        max_inflight: cfg.max_inflight as u64,
        batch_size: cfg.batch_size,
        idle_tick_s: cfg.idle_tick.as_secs(),
        channel_interval_s: cfg.channel_interval.as_secs(),
        sidecar_fallback_enabled: cfg.sidecar_fallback_enabled,
        sidecar_fallback_min_interval_s: cfg.sidecar_fallback_min_interval.as_secs(),
        sidecar_fallback_max_per_hour: cfg.sidecar_fallback_max_per_hour,
        raw: RefresherSettingsRaw {
            dispatch_delay_ms: raw.dispatch_delay_ms,
            max_inflight: raw.max_inflight,
            batch_size: raw.batch_size,
            idle_tick_s: raw.idle_tick_s,
            channel_interval_s: raw.channel_interval_s,
            sidecar_fallback_enabled: raw.sidecar_fallback_enabled,
            sidecar_fallback_min_interval_s: raw.sidecar_fallback_min_interval_s,
            sidecar_fallback_max_per_hour: raw.sidecar_fallback_max_per_hour,
        },
    }))
}

#[derive(Debug, Serialize)]
pub struct RefresherSettings {
    pub dispatch_delay_ms: u64,
    pub max_inflight: u64,
    pub batch_size: i64,
    pub idle_tick_s: u64,
    pub channel_interval_s: u64,
    /// Whether the refresher is allowed to fall back to the
    /// youtubei.js discovery sidecar when an RSS poll fails.
    pub sidecar_fallback_enabled: bool,
    /// Per-source minimum interval (seconds) between successive
    /// sidecar fallbacks for the same source.
    pub sidecar_fallback_min_interval_s: u64,
    /// Aggregate per-hour cap on sidecar fallbacks across the whole
    /// refresher. `0` = unlimited (per-source still applies).
    pub sidecar_fallback_max_per_hour: u64,
    /// Raw string values from `app_config` (or null if the key is
    /// unset). When a raw value disagrees with the effective field
    /// above, it was rejected by range validation in
    /// `RefresherConfig::load_with_raw`.
    pub raw: RefresherSettingsRaw,
}

#[derive(Debug, Serialize)]
pub struct RefresherSettingsRaw {
    pub dispatch_delay_ms: Option<String>,
    pub max_inflight: Option<String>,
    pub batch_size: Option<String>,
    pub idle_tick_s: Option<String>,
    pub channel_interval_s: Option<String>,
    pub sidecar_fallback_enabled: Option<String>,
    pub sidecar_fallback_min_interval_s: Option<String>,
    pub sidecar_fallback_max_per_hour: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct UpdateRefresherSettings {
    #[serde(default)]
    pub dispatch_delay_ms: Option<u64>,
    #[serde(default)]
    pub max_inflight: Option<u64>,
    #[serde(default)]
    pub batch_size: Option<i64>,
    #[serde(default)]
    pub idle_tick_s: Option<u64>,
    #[serde(default)]
    pub channel_interval_s: Option<u64>,
    #[serde(default)]
    pub sidecar_fallback_enabled: Option<bool>,
    #[serde(default)]
    pub sidecar_fallback_min_interval_s: Option<u64>,
    #[serde(default)]
    pub sidecar_fallback_max_per_hour: Option<u64>,
}

/// `PUT /api/admin/feed-refresher/settings` — parent-only.
///
/// Updates any subset of the live refresher tunables. The refresher
/// loop re-reads `app_config` on its next iteration, so changes take
/// effect within `idle_tick_s` seconds without a restart. Range checks
/// here mirror the ones in `RefresherConfig::load` so that values
/// rejected at write time can't sneak in via direct SQL.
pub async fn admin_put_refresher_settings(
    State(state): State<AppState>,
    Json(body): Json<UpdateRefresherSettings>,
) -> AppResult<Json<RefresherSettings>> {
    use crate::error::AppError;
    use crate::services::feed_refresher::{
        KEY_BATCH_SIZE, KEY_CHANNEL_INTERVAL_S, KEY_DISPATCH_DELAY_MS, KEY_IDLE_TICK_S,
        KEY_MAX_INFLIGHT, KEY_SIDECAR_FALLBACK_ENABLED, KEY_SIDECAR_FALLBACK_MAX_PER_HOUR,
        KEY_SIDECAR_FALLBACK_MIN_INTERVAL_S, RANGE_BATCH_SIZE, RANGE_CHANNEL_INTERVAL_S,
        RANGE_DISPATCH_DELAY_MS, RANGE_IDLE_TICK_S, RANGE_MAX_INFLIGHT,
        RANGE_SIDECAR_FALLBACK_MAX_PER_HOUR, RANGE_SIDECAR_FALLBACK_MIN_INTERVAL_S,
    };
    use crate::services::setup::set_config_value;

    if let Some(v) = body.dispatch_delay_ms {
        if !RANGE_DISPATCH_DELAY_MS.contains(&v) {
            return Err(AppError::BadRequest(format!(
                "dispatch_delay_ms must be {}..={}",
                RANGE_DISPATCH_DELAY_MS.start(),
                RANGE_DISPATCH_DELAY_MS.end()
            )));
        }
        set_config_value(&state.db, KEY_DISPATCH_DELAY_MS, &v.to_string()).await?;
    }
    if let Some(v) = body.max_inflight {
        if !RANGE_MAX_INFLIGHT.contains(&v) {
            return Err(AppError::BadRequest(format!(
                "max_inflight must be {}..={}",
                RANGE_MAX_INFLIGHT.start(),
                RANGE_MAX_INFLIGHT.end()
            )));
        }
        set_config_value(&state.db, KEY_MAX_INFLIGHT, &v.to_string()).await?;
    }
    if let Some(v) = body.batch_size {
        if !RANGE_BATCH_SIZE.contains(&v) {
            return Err(AppError::BadRequest(format!(
                "batch_size must be {}..={}",
                RANGE_BATCH_SIZE.start(),
                RANGE_BATCH_SIZE.end()
            )));
        }
        set_config_value(&state.db, KEY_BATCH_SIZE, &v.to_string()).await?;
    }
    if let Some(v) = body.idle_tick_s {
        if !RANGE_IDLE_TICK_S.contains(&v) {
            return Err(AppError::BadRequest(format!(
                "idle_tick_s must be {}..={}",
                RANGE_IDLE_TICK_S.start(),
                RANGE_IDLE_TICK_S.end()
            )));
        }
        set_config_value(&state.db, KEY_IDLE_TICK_S, &v.to_string()).await?;
    }
    if let Some(v) = body.channel_interval_s {
        if !RANGE_CHANNEL_INTERVAL_S.contains(&v) {
            return Err(AppError::BadRequest(format!(
                "channel_interval_s must be {}..={}",
                RANGE_CHANNEL_INTERVAL_S.start(),
                RANGE_CHANNEL_INTERVAL_S.end()
            )));
        }
        set_config_value(&state.db, KEY_CHANNEL_INTERVAL_S, &v.to_string()).await?;
    }
    if let Some(v) = body.sidecar_fallback_enabled {
        // Boolean — no range check.
        set_config_value(
            &state.db,
            KEY_SIDECAR_FALLBACK_ENABLED,
            if v { "true" } else { "false" },
        )
        .await?;
    }
    if let Some(v) = body.sidecar_fallback_min_interval_s {
        if !RANGE_SIDECAR_FALLBACK_MIN_INTERVAL_S.contains(&v) {
            return Err(AppError::BadRequest(format!(
                "sidecar_fallback_min_interval_s must be {}..={}",
                RANGE_SIDECAR_FALLBACK_MIN_INTERVAL_S.start(),
                RANGE_SIDECAR_FALLBACK_MIN_INTERVAL_S.end()
            )));
        }
        set_config_value(
            &state.db,
            KEY_SIDECAR_FALLBACK_MIN_INTERVAL_S,
            &v.to_string(),
        )
        .await?;
    }
    if let Some(v) = body.sidecar_fallback_max_per_hour {
        if !RANGE_SIDECAR_FALLBACK_MAX_PER_HOUR.contains(&v) {
            return Err(AppError::BadRequest(format!(
                "sidecar_fallback_max_per_hour must be {}..={}",
                RANGE_SIDECAR_FALLBACK_MAX_PER_HOUR.start(),
                RANGE_SIDECAR_FALLBACK_MAX_PER_HOUR.end()
            )));
        }
        set_config_value(&state.db, KEY_SIDECAR_FALLBACK_MAX_PER_HOUR, &v.to_string()).await?;
    }
    admin_get_refresher_settings(State(state)).await
}

/// `GET /api/admin/feed-sources` — parent-only diagnostics.
///
/// Returns one row per cached source with its poll bookkeeping
/// (last_polled_at, last_success_at, last_error, consecutive_errors,
/// next_poll_at) plus the number of items currently held. Surfaces
/// poll health without requiring SQLite access.
pub async fn admin_list_sources(
    State(state): State<AppState>,
) -> AppResult<Json<Vec<feed_cache::FeedSourceStatus>>> {
    let rows = feed_cache::list_source_status(&state.db).await?;
    Ok(Json(rows))
}

/// Capacity / utilisation summary for the parent diagnostics UI.
///
/// Combines the raw counts from `feed_cache::capacity_counts` with
/// the effective refresher config to surface "are we keeping up?" as
/// a single number, plus the inputs that produced it. The UI uses
/// this to colour-code the panel and prompt the operator to lower
/// `dispatch_delay_ms` or raise `channel_interval_s` once utilisation
/// climbs past ~70%.
#[derive(Debug, Serialize)]
pub struct RefresherCapacity {
    /// Number of allowlisted-channel + allowlisted-playlist rows we
    /// currently track.
    pub total_sources: i64,
    /// Sources whose `next_poll_at` is in the past *right now*. A
    /// healthy refresher keeps this near zero; a persistent non-zero
    /// value means the dispatcher can't keep up at current tunables.
    pub queue_depth: i64,
    /// Actual RSS polls dispatched in the last hour (any source with
    /// `last_polled_at >= now - 3600`). Imperfect — a source might
    /// have been polled multiple times in the window but we only
    /// store the most recent timestamp — but it's a good "is the
    /// loop actually doing work?" sanity check.
    pub polls_last_hour: i64,
    /// Sidecar fallbacks dispatched in the last hour. Mirrors the
    /// number the aggregate-cap eligibility check sees so the
    /// operator can validate the cap is working.
    pub sidecar_fallbacks_last_hour: i64,
    /// Theoretical maximum polls per hour the dispatcher could
    /// achieve at the current `dispatch_delay_ms`. Computed as
    /// `3600 / (dispatch_delay_ms / 1000)`.
    pub theoretical_polls_per_hour: u64,
    /// Polls we'd need per hour to honour `channel_interval_s` for
    /// every source: `total_sources / (channel_interval_s / 3600)`.
    pub required_polls_per_hour: f64,
    /// `required / theoretical * 100`, capped at 999. A reading
    /// above ~70 means the dispatcher is approaching saturation; a
    /// reading above 100 means the queue can't drain.
    pub utilization_pct: f64,
}

/// `GET /api/admin/feed-refresher/capacity` — parent-only.
pub async fn admin_get_refresher_capacity(
    State(state): State<AppState>,
) -> AppResult<Json<RefresherCapacity>> {
    let now = chrono::Utc::now().timestamp();
    let counts = feed_cache::capacity_counts(&state.db, now).await?;
    let cfg = crate::services::feed_refresher::RefresherConfig::load(&state.db).await;

    // Dispatch delay floor of 1ms keeps the division well-defined
    // even under pathological config.
    let dispatch_secs = (cfg.dispatch_delay.as_millis() as f64 / 1000.0).max(0.001);
    let theoretical = (3600.0 / dispatch_secs).floor() as u64;

    let interval_secs = cfg.channel_interval.as_secs().max(1) as f64;
    let required = counts.total_sources as f64 * 3600.0 / interval_secs;

    let util = if theoretical == 0 {
        0.0
    } else {
        // Round to one decimal place server-side so two saves that
        // produce mathematically-equivalent settings serialise to the
        // same string, and the UI's `<70` threshold isn't sensitive
        // to float fuzz like 69.99999999999.
        let raw = (required / theoretical as f64 * 100.0).min(999.0);
        (raw * 10.0).round() / 10.0
    };
    let required = (required * 10.0).round() / 10.0;

    Ok(Json(RefresherCapacity {
        total_sources: counts.total_sources,
        queue_depth: counts.queue_depth,
        polls_last_hour: counts.polls_last_hour,
        sidecar_fallbacks_last_hour: counts.sidecar_fallbacks_last_hour,
        theoretical_polls_per_hour: theoretical,
        required_polls_per_hour: required,
        utilization_pct: util,
    }))
}

/// `GET /api/feed/new-videos`.
///
/// Reads from the `feed_source_items` cache populated by the
/// [`crate::services::feed_refresher`] background task. The handler
/// performs no network I/O and no per-item access-control checks; both
/// are folded into the single SQL query inside
/// [`feed_cache::feed_for_child`].
pub async fn new_videos(
    State(state): State<AppState>,
    current: CurrentAccount,
) -> AppResult<Json<Vec<NewVideoItem>>> {
    let items = feed_cache::feed_for_child(&state.db, current.id, NEW_VIDEOS_LIMIT).await?;
    Ok(Json(items))
}

// ---------------------------------------------------------------------------
// Up-next queue
// ---------------------------------------------------------------------------

/// Default queue length for `/api/feed/up-next`.
const UP_NEXT_DEFAULT_LIMIT: usize = 10;

#[derive(Debug, Deserialize)]
pub struct UpNextQuery {
    /// Source context. Recognised values:
    ///
    /// - `playlist:<id>` — videos from a `child_playlists` row (own or
    ///   library) ordered by `position`.
    /// - `channel:<id>` — channel uploads, drawn from the discovery sidecar.
    /// - `video:<id>` — fall back to "more from the same channel" or
    ///   the new-videos feed if no channel context is available.
    /// - missing — returns the new-videos feed.
    #[serde(default)]
    pub from: Option<String>,
    /// Currently-playing video ID. Excluded from the result.
    #[serde(default)]
    pub current_video: Option<String>,
    #[serde(default)]
    pub limit: Option<usize>,
}

#[derive(Debug, Serialize, Clone)]
pub struct UpNextItem {
    pub video_id: String,
    pub title: String,
    pub channel_id: Option<String>,
    pub channel_title: Option<String>,
    pub thumbnail_url: Option<String>,
}

/// `GET /api/feed/up-next`.
pub async fn up_next(
    State(state): State<AppState>,
    current: CurrentAccount,
    Query(q): Query<UpNextQuery>,
) -> AppResult<Json<Vec<UpNextItem>>> {
    let limit = q.limit.unwrap_or(UP_NEXT_DEFAULT_LIMIT).clamp(1, 50);

    // Parse `from`.
    let (kind, id) = parse_from(q.from.as_deref());

    // Only treat as a playlist context when *both* the kind and id
    // parsed cleanly. `from=playlist` (no id) falls through to the
    // new-videos pool and must still get the watched-filter applied.
    let is_playlist_ctx = matches!((kind, id), (Some("playlist"), Some(_)));
    let raw: Vec<UpNextItem> = match (kind, id) {
        (Some("playlist"), Some(playlist_id)) => {
            up_next_from_playlist(&state, current.id, playlist_id, q.current_video.as_deref())
                .await?
        }
        (Some("channel"), Some(channel_id)) => {
            up_next_from_channel(&state, current.id, channel_id).await?
        }
        _ => up_next_from_new_videos(&state, current.id, limit).await?,
    };

    // For non-playlist contexts, exclude videos the child has already
    // watched so the queue actually rotates instead of resurfacing the
    // same items. Playlist contexts preserve order: the user explicitly
    // opened that list and may want to re-watch in sequence.
    let watched: HashSet<String> = if is_playlist_ctx {
        HashSet::new()
    } else {
        child_watched_video_ids(&state.db, current.id)
            .await
            .unwrap_or_default()
    };

    // Drop the current video, watched items (non-playlist), and
    // access-control failures.
    let mut out = Vec::with_capacity(limit);
    for item in raw {
        if Some(&item.video_id) == q.current_video.as_ref() {
            continue;
        }
        if watched.contains(&item.video_id) {
            continue;
        }
        if can_child_view(
            &state.db,
            current.id,
            &item.video_id,
            item.channel_id.as_deref(),
            &[],
        )
        .await
        .unwrap_or(false)
        {
            out.push(item);
            if out.len() >= limit {
                break;
            }
        }
    }
    Ok(Json(out))
}

/// Build a deterministic RNG keyed to `(child_id, today)` so the
/// up-next list stays stable for a given child within a calendar day
/// and rotates naturally each day. Without this, every page load
/// reshuffles the list, which is jarring when the user navigates
/// back-and-forth between videos.
fn daily_rng(child_id: i64) -> StdRng {
    // Use local-time day boundary so the queue rotates at the user's
    // midnight (matches `usage.rs` which also keys daily limits off
    // `chrono::Local`).
    use chrono::Datelike;
    let day = chrono::Local::now().date_naive().num_days_from_ce() as i64;
    let mut seed = [0u8; 32];
    seed[..8].copy_from_slice(&(child_id as u64).to_le_bytes());
    seed[8..16].copy_from_slice(&(day as u64).to_le_bytes());
    StdRng::from_seed(seed)
}

/// Returns the set of video IDs the child has any watch_history row for.
async fn child_watched_video_ids(
    db: &sqlx::SqlitePool,
    child_id: i64,
) -> AppResult<HashSet<String>> {
    // Recency cap: a child's watch_history can grow unbounded over
    // time, so bound the query to the most recently watched 500 rows.
    // `watch_history` has UNIQUE(child_account_id, video_id), so no
    // GROUP BY is needed.
    let rows: Vec<(String,)> = sqlx::query_as(
        "SELECT video_id FROM watch_history \
         WHERE child_account_id = ? \
         ORDER BY last_watched_at DESC \
         LIMIT 500",
    )
    .bind(child_id)
    .fetch_all(db)
    .await?;
    Ok(rows.into_iter().map(|(v,)| v).collect())
}

fn parse_from(from: Option<&str>) -> (Option<&str>, Option<&str>) {
    let Some(s) = from else {
        return (None, None);
    };
    let mut iter = s.splitn(2, ':');
    let kind = iter.next();
    let id = iter.next();
    (kind, id)
}

async fn up_next_from_playlist(
    state: &AppState,
    child_id: i64,
    playlist_id: &str,
    current_video: Option<&str>,
) -> AppResult<Vec<UpNextItem>> {
    // Match either by primary-key id or by youtube_playlist_id.
    let row: Option<(i64,)> = sqlx::query_as(
        "SELECT id FROM child_playlists \
         WHERE child_account_id = ? AND is_deleted = 0 \
           AND (CAST(id AS TEXT) = ? OR youtube_playlist_id = ?)",
    )
    .bind(child_id)
    .bind(playlist_id)
    .bind(playlist_id)
    .fetch_optional(&state.db)
    .await?;
    let Some((local_id,)) = row else {
        return Ok(Vec::new());
    };

    let rows: Vec<(String, String, Option<String>, Option<String>)> = sqlx::query_as(
        "SELECT video_id, video_title, video_thumbnail_url, channel_title \
         FROM child_playlist_videos WHERE playlist_id = ? ORDER BY position",
    )
    .bind(local_id)
    .fetch_all(&state.db)
    .await?;

    let items: Vec<UpNextItem> = rows
        .into_iter()
        .map(|(video_id, title, thumb, ch)| UpNextItem {
            video_id,
            title,
            channel_id: None,
            channel_title: ch,
            thumbnail_url: thumb,
        })
        .collect();

    // Treat `current_video` as a cursor: return items after it, wrapping
    // around so the queue still has something if the current item is at
    // the end of the list. When there's no current video, return the
    // full list in order.
    let Some(current) = current_video else {
        return Ok(items);
    };
    let Some(idx) = items.iter().position(|it| it.video_id == current) else {
        return Ok(items);
    };
    let mut out = Vec::with_capacity(items.len().saturating_sub(1));
    out.extend(items.iter().skip(idx + 1).cloned());
    out.extend(items.iter().take(idx).cloned());
    Ok(out)
}

async fn up_next_from_channel(
    state: &AppState,
    child_id: i64,
    channel_id: &str,
) -> AppResult<Vec<UpNextItem>> {
    // Prefer the cached items populated by the feed refresher; this
    // avoids a sidecar round-trip on every up-next request and reuses
    // the same data the new-videos feed shows.
    let rows: Vec<UpNextRow> = sqlx::query_as(
        "SELECT video_id, title, channel_id, channel_title, thumbnail_url \
           FROM feed_source_items \
          WHERE kind = ? AND source_id = ? \
          ORDER BY COALESCE(published_at, 0) DESC \
          LIMIT 25",
    )
    .bind(feed_cache::KIND_CHANNEL)
    .bind(channel_id)
    .fetch_all(&state.db)
    .await?;
    let mut items: Vec<UpNextItem> = rows
        .into_iter()
        .map(|r| UpNextItem {
            video_id: r.video_id,
            title: r.title,
            channel_id: r.channel_id.or_else(|| Some(channel_id.to_string())),
            channel_title: r.channel_title,
            thumbnail_url: r.thumbnail_url,
        })
        .collect();
    // Shuffle so consecutive visits don't surface the same top-N
    // uploads. Seed deterministically so the order is stable within a
    // day for a given child + channel and rotates daily.
    let mut rng = daily_rng(child_id ^ channel_seed(channel_id));
    items.shuffle(&mut rng);
    Ok(items)
}

/// One row pulled from `feed_source_items` for the up-next builder.
/// Pulled out into a struct to satisfy clippy's `type_complexity` lint
/// and to make the column ordering explicit.
#[derive(sqlx::FromRow)]
struct UpNextRow {
    video_id: String,
    title: String,
    channel_id: Option<String>,
    channel_title: Option<String>,
    thumbnail_url: Option<String>,
}

/// Hash a channel ID into a stable `i64` so we can mix it into the
/// daily RNG seed (so two different channels for the same child shuffle
/// differently).
fn channel_seed(channel_id: &str) -> i64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    channel_id.hash(&mut h);
    h.finish() as i64
}

async fn up_next_from_new_videos(
    state: &AppState,
    child_id: i64,
    limit: usize,
) -> AppResult<Vec<UpNextItem>> {
    // Pull items from the refresher's cache rather than fanning out to
    // the sidecar on every request. Group them into per-source buckets
    // so we can interleave (round-robin) instead of front-loading any
    // single channel's uploads.
    let channels = child_allowlisted_channel_ids(&state.db, child_id).await?;
    let playlists = child_allowlisted_playlist_ids(&state.db, child_id).await?;

    let mut buckets: Vec<Vec<UpNextItem>> = Vec::new();
    for channel_id in &channels {
        let rows: Vec<UpNextRow> = sqlx::query_as(
            "SELECT video_id, title, channel_id, channel_title, thumbnail_url \
               FROM feed_source_items \
              WHERE kind = ? AND source_id = ? \
              ORDER BY COALESCE(published_at, 0) DESC \
              LIMIT 5",
        )
        .bind(feed_cache::KIND_CHANNEL)
        .bind(channel_id)
        .fetch_all(&state.db)
        .await
        .unwrap_or_default();
        let bucket: Vec<UpNextItem> = rows
            .into_iter()
            .map(|r| UpNextItem {
                video_id: r.video_id,
                title: r.title,
                channel_id: r.channel_id.or_else(|| Some(channel_id.clone())),
                channel_title: r.channel_title,
                thumbnail_url: r.thumbnail_url,
            })
            .collect();
        if !bucket.is_empty() {
            buckets.push(bucket);
        }
    }
    for playlist_id in &playlists {
        let rows: Vec<UpNextRow> = sqlx::query_as(
            "SELECT video_id, title, channel_id, channel_title, thumbnail_url \
               FROM feed_source_items \
              WHERE kind = ? AND source_id = ? \
              ORDER BY COALESCE(published_at, 0) DESC \
              LIMIT 5",
        )
        .bind(feed_cache::KIND_PLAYLIST)
        .bind(playlist_id)
        .fetch_all(&state.db)
        .await
        .unwrap_or_default();
        let bucket: Vec<UpNextItem> = rows
            .into_iter()
            .map(|r| UpNextItem {
                video_id: r.video_id,
                title: r.title,
                channel_id: r.channel_id,
                channel_title: r.channel_title,
                thumbnail_url: r.thumbnail_url,
            })
            .collect();
        if !bucket.is_empty() {
            buckets.push(bucket);
        }
    }

    // Shuffle each bucket so the picked items aren't always the newest
    // few, then round-robin across buckets so the result mixes sources.
    // Daily-deterministic so the home queue is stable for a given
    // child within a day.
    let mut rng = daily_rng(child_id);
    for bucket in &mut buckets {
        bucket.shuffle(&mut rng);
    }
    buckets.shuffle(&mut rng);

    // Round-robin pop until we have ~2× the caller's limit. The
    // caller (`up_next`) trims further after current_video/watched/
    // access-control filtering, so 2× gives enough headroom while
    // skipping wasted work for users with many allowlisted sources.
    let target = limit.saturating_mul(2).max(UP_NEXT_DEFAULT_LIMIT);
    let mut out: Vec<UpNextItem> = Vec::new();
    let mut exhausted = 0usize;
    while exhausted < buckets.len() && out.len() < target {
        exhausted = 0;
        for bucket in &mut buckets {
            if out.len() >= target {
                break;
            }
            if let Some(it) = bucket.pop() {
                out.push(it);
            } else {
                exhausted += 1;
            }
        }
    }
    Ok(out)
}
