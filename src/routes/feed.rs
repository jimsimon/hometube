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
}

/// `GET /api/feed/continue-watching`.
pub async fn continue_watching(
    State(state): State<AppState>,
    current: CurrentAccount,
) -> AppResult<Json<Vec<ContinueWatchingItem>>> {
    let rows: Vec<ContinueWatchingItem> = sqlx::query_as(
        "SELECT video_id, video_title, video_thumbnail_url, channel_title, \
                duration_seconds, progress_seconds, last_watched_at \
         FROM watch_history \
         WHERE child_account_id = ? \
         ORDER BY last_watched_at DESC \
         LIMIT ?",
    )
    .bind(current.id)
    .bind(CONTINUE_LIMIT)
    .fetch_all(&state.db)
    .await?;

    // Filter through access control. We don't know the channel/playlist
    // for historical entries, so we just check the basic allowlist
    // tables (which is what `can_child_view` does).
    let mut filtered = Vec::with_capacity(rows.len());
    for row in rows {
        if can_child_view(&state.db, current.id, &row.video_id, None, &[])
            .await
            .unwrap_or(false)
        {
            filtered.push(row);
        }
    }
    Ok(Json(filtered))
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
        raw: RefresherSettingsRaw {
            dispatch_delay_ms: raw.dispatch_delay_ms,
            max_inflight: raw.max_inflight,
            batch_size: raw.batch_size,
            idle_tick_s: raw.idle_tick_s,
            channel_interval_s: raw.channel_interval_s,
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
        KEY_MAX_INFLIGHT,
    };
    use crate::services::setup::set_config_value;

    if let Some(v) = body.dispatch_delay_ms {
        if !(50..=600_000).contains(&v) {
            return Err(AppError::BadRequest(
                "dispatch_delay_ms must be 50..=600000".into(),
            ));
        }
        set_config_value(&state.db, KEY_DISPATCH_DELAY_MS, &v.to_string()).await?;
    }
    if let Some(v) = body.max_inflight {
        if !(1..=64).contains(&v) {
            return Err(AppError::BadRequest("max_inflight must be 1..=64".into()));
        }
        set_config_value(&state.db, KEY_MAX_INFLIGHT, &v.to_string()).await?;
    }
    if let Some(v) = body.batch_size {
        if !(1..=500).contains(&v) {
            return Err(AppError::BadRequest("batch_size must be 1..=500".into()));
        }
        set_config_value(&state.db, KEY_BATCH_SIZE, &v.to_string()).await?;
    }
    if let Some(v) = body.idle_tick_s {
        if !(1..=3600).contains(&v) {
            return Err(AppError::BadRequest("idle_tick_s must be 1..=3600".into()));
        }
        set_config_value(&state.db, KEY_IDLE_TICK_S, &v.to_string()).await?;
    }
    if let Some(v) = body.channel_interval_s {
        if !(60..=86_400).contains(&v) {
            return Err(AppError::BadRequest(
                "channel_interval_s must be 60..=86400".into(),
            ));
        }
        set_config_value(&state.db, KEY_CHANNEL_INTERVAL_S, &v.to_string()).await?;
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
