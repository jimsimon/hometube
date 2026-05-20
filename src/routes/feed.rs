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

use axum::{
    extract::{Query, State},
    Json,
};
use serde::{Deserialize, Serialize};

use crate::error::AppResult;
use crate::middleware::auth::CurrentAccount;
use crate::services::access::can_child_view;
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
        KEY_MAX_INFLIGHT, RANGE_BATCH_SIZE, RANGE_CHANNEL_INTERVAL_S, RANGE_DISPATCH_DELAY_MS,
        RANGE_IDLE_TICK_S, RANGE_MAX_INFLIGHT,
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

    let raw: Vec<UpNextItem> = match (kind, id) {
        (Some("playlist"), Some(playlist_id)) => {
            up_next_from_playlist(&state, current.id, playlist_id).await?
        }
        (Some("channel"), Some(channel_id)) => up_next_from_channel(&state, channel_id).await?,
        _ => up_next_from_new_videos(&state, current.id).await?,
    };

    // Drop the current video and access-control failures.
    let mut out = Vec::with_capacity(limit);
    for item in raw {
        if Some(&item.video_id) == q.current_video.as_ref() {
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
    Ok(rows
        .into_iter()
        .map(|(video_id, title, thumb, ch)| UpNextItem {
            video_id,
            title,
            channel_id: None,
            channel_title: ch,
            thumbnail_url: thumb,
        })
        .collect())
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

async fn up_next_from_channel(state: &AppState, channel_id: &str) -> AppResult<Vec<UpNextItem>> {
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
    Ok(rows
        .into_iter()
        .map(|r| UpNextItem {
            video_id: r.video_id,
            title: r.title,
            channel_id: r.channel_id.or_else(|| Some(channel_id.to_string())),
            channel_title: r.channel_title,
            thumbnail_url: r.thumbnail_url,
        })
        .collect())
}

async fn up_next_from_new_videos(state: &AppState, child_id: i64) -> AppResult<Vec<UpNextItem>> {
    // Reuse the new-videos feed builder; the up-next list is exactly
    // "new videos minus the one currently playing", which the caller
    // applies in `up_next`.
    let items = feed_cache::feed_for_child(&state.db, child_id, 50).await?;
    Ok(items
        .into_iter()
        .map(|it| UpNextItem {
            video_id: it.video_id,
            title: it.title,
            channel_id: it.channel_id,
            channel_title: it.channel_title,
            thumbnail_url: it.thumbnail_url,
        })
        .collect())
}
