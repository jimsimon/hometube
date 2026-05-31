//! Child home-page feed routes.
//!
//! Three read-only endpoints back the child UI:
//!
//! - `GET /api/feed/continue-watching` — in-progress items from
//!   `watch_history` (rows that have NOT yet reached the
//!   [`is_effectively_finished`] threshold), filtered through access
//!   control to drop videos the parent has since revoked.
//! - `GET /api/feed/watch-again` — finished items from `watch_history`
//!   (rows that HAVE reached [`is_effectively_finished`]), ordered by
//!   most recently watched, also access-filtered. Within each feed's
//!   recency window (the top `LIMIT` rows by `last_watched_at`), the
//!   in-progress/finished partition is disjoint — but the two feeds
//!   each fetch their own top-N independently before partitioning, so
//!   a child with many recent rows of one kind may see fewer than N
//!   items in the other.
//! - `GET /api/feed/new-videos` — fresh uploads from each allowlisted
//!   channel, deduped + sorted.
//! - `GET /api/feed/up-next` — the next videos to play after the one
//!   currently on screen, given a `from=channel:ID|video:ID`
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
use crate::services::access::{can_child_view, child_allowlisted_channel_ids};
use crate::services::feed_cache;
use crate::state::AppState;

/// Default limit for "continue watching".
const CONTINUE_LIMIT: i64 = 20;
/// Default limit for "watch again".
const WATCH_AGAIN_LIMIT: i64 = 20;
/// A video is considered "finished" (dropped from continue-watching,
/// promoted to watch-again) once the saved position is within this many
/// seconds of the end. Picks up the typical 5–15s of outro/credits that
/// most viewers don't sit through but the player still records before
/// pause/ended fires.
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
    /// Source publish date as unix seconds, joined from
    /// `channel_videos`. `None` when the video has no archive row.
    pub published_at: Option<i64>,
    /// Skipped from the JSON wire shape (the row component doesn't need
    /// it) but required server-side so the access check can recognise
    /// channel-allowlisted videos.
    #[serde(skip_serializing)]
    pub channel_id: Option<String>,
}

/// `GET /api/feed/continue-watching`.
///
/// Returns in-progress videos — anything in `watch_history` that is
/// not yet "effectively finished" (see [`is_effectively_finished`]).
/// Completed videos are surfaced separately by `watch_again` so the
/// two home rows show disjoint sets.
pub async fn continue_watching(
    State(state): State<AppState>,
    current: CurrentAccount,
) -> AppResult<Json<Vec<ContinueWatchingItem>>> {
    let rows: Vec<ContinueWatchingItem> = sqlx::query_as(
        "SELECT wh.video_id, v.title AS video_title, \
                v.thumbnail_url AS video_thumbnail_url, \
                ch.channel_title, \
                v.duration_seconds, wh.progress_seconds, wh.last_watched_at, \
                COALESCE( \
                    (SELECT cv.published_at FROM channel_videos cv \
                      WHERE cv.video_id = wh.video_id AND cv.channel_id = v.channel_id \
                        AND cv.published_at IS NOT NULL LIMIT 1), \
                    (SELECT cv.published_at FROM channel_videos cv \
                      WHERE cv.video_id = wh.video_id AND cv.published_at IS NOT NULL \
                      ORDER BY cv.last_seen_at DESC LIMIT 1) \
                ) AS published_at, \
                v.channel_id \
         FROM watch_history wh \
         JOIN videos v ON v.video_id = wh.video_id \
         LEFT JOIN channels ch ON ch.channel_id = v.channel_id \
         WHERE wh.child_account_id = ? \
         ORDER BY wh.last_watched_at DESC \
         LIMIT ?",
    )
    .bind(current.id)
    .bind(CONTINUE_LIMIT)
    .fetch_all(&state.db)
    .await?;

    filter_watch_history_rows(&state.db, current.id, rows, |r| {
        !is_effectively_finished(r.progress_seconds, r.duration_seconds)
    })
    .await
}

/// Common pipeline for `continue-watching` and `watch-again`:
///
/// 1. Resolve `channel_id` for rows whose `videos.channel_id` is still
///    NULL via a single batched query against `channel_videos` — see
///    [`lookup_channel_ids_for_videos`] for when this can happen.
/// 2. Apply `keep` (the per-feed completion predicate).
/// 3. Per-row access check via `can_child_view`, passing the (possibly
///    fallback-resolved) channel_id so channel-allowlisted videos still
///    surface.
///
/// Per-row access checks are intentional — the access rules involve
/// blocked/hidden/allowlist tables across multiple dimensions and the
/// row count is bounded (≤20). Folding them into SQL would not capture
/// channel allowlist hits without first materialising the channel_id,
/// which we already do here.
async fn filter_watch_history_rows<T, F>(
    db: &sqlx::SqlitePool,
    child_id: i64,
    rows: Vec<T>,
    keep: F,
) -> AppResult<Json<Vec<T>>>
where
    T: WatchHistoryRow,
    F: Fn(&T) -> bool,
{
    // Build the fallback-lookup set from rows that both (a) pass the
    // per-feed predicate and (b) are missing a stored channel_id. No
    // point asking channel_videos about rows we'll drop anyway.
    let unresolved_ids: Vec<&str> = rows
        .iter()
        .filter(|r| keep(r) && r.channel_id().is_none())
        .map(|r| r.video_id())
        .collect();
    let unresolved_map = match lookup_channel_ids_for_videos(db, &unresolved_ids).await {
        Ok(map) => map,
        Err(err) => {
            // Don't fail the whole row over a transient cache lookup
            // problem — fall back to "no fallback resolution" so freshly
            // upserted rows (which have channel_id stored directly) and
            // individually-allowlisted videos still surface. But do log
            // it: silently reverting to the pre-fix broken behaviour is
            // exactly what bit us before.
            tracing::warn!(
                error = %err,
                "watch_history feed: channel_videos lookup failed; rows with unresolved channel ids may be hidden"
            );
            std::collections::HashMap::new()
        }
    };

    let mut filtered = Vec::with_capacity(rows.len());
    for row in rows {
        if !keep(&row) {
            continue;
        }
        let channel_id = row
            .channel_id()
            .or_else(|| unresolved_map.get(row.video_id()).map(String::as_str));
        if can_child_view(db, child_id, row.video_id(), channel_id)
            .await
            .unwrap_or(false)
        {
            filtered.push(row);
        }
    }
    Ok(Json(filtered))
}

/// Common accessors used by [`filter_watch_history_rows`].
trait WatchHistoryRow {
    fn video_id(&self) -> &str;
    fn channel_id(&self) -> Option<&str>;
}

impl WatchHistoryRow for ContinueWatchingItem {
    fn video_id(&self) -> &str {
        &self.video_id
    }
    fn channel_id(&self) -> Option<&str> {
        self.channel_id.as_deref()
    }
}

impl WatchHistoryRow for WatchAgainItem {
    fn video_id(&self) -> &str {
        &self.video_id
    }
    fn channel_id(&self) -> Option<&str> {
        self.channel_id.as_deref()
    }
}

#[derive(Debug, Serialize, sqlx::FromRow, Clone)]
pub struct WatchAgainItem {
    pub video_id: String,
    pub video_title: String,
    pub video_thumbnail_url: Option<String>,
    pub channel_title: Option<String>,
    pub duration_seconds: Option<i64>,
    pub last_watched_at: i64,
    /// Source publish date as unix seconds, joined from
    /// `channel_videos`. `None` when the video has no archive row.
    pub published_at: Option<i64>,
    /// Server-side only: rows in this feed are by definition finished,
    /// so the UI doesn't render a progress bar. Kept on the struct for
    /// SQL hydration / future use, but omitted from the wire shape.
    #[serde(skip_serializing)]
    pub progress_seconds: i64,
    /// See `ContinueWatchingItem::channel_id` — needed server-side for
    /// the channel-allowlist access check.
    #[serde(skip_serializing)]
    pub channel_id: Option<String>,
}

/// `GET /api/feed/watch-again`.
///
/// Returns videos the child has finished (see [`is_effectively_finished`]),
/// ordered by most recently watched. Within the recency window, this
/// is the disjoint complement of `continue-watching` — see the module
/// docstring for the caveat about each feed sampling its own top-N.
pub async fn watch_again(
    State(state): State<AppState>,
    current: CurrentAccount,
) -> AppResult<Json<Vec<WatchAgainItem>>> {
    let rows: Vec<WatchAgainItem> = sqlx::query_as(
        "SELECT wh.video_id, v.title AS video_title, \
                v.thumbnail_url AS video_thumbnail_url, \
                ch.channel_title, \
                v.duration_seconds, wh.progress_seconds, wh.last_watched_at, \
                COALESCE( \
                    (SELECT cv.published_at FROM channel_videos cv \
                      WHERE cv.video_id = wh.video_id AND cv.channel_id = v.channel_id \
                        AND cv.published_at IS NOT NULL LIMIT 1), \
                    (SELECT cv.published_at FROM channel_videos cv \
                      WHERE cv.video_id = wh.video_id AND cv.published_at IS NOT NULL \
                      ORDER BY cv.last_seen_at DESC LIMIT 1) \
                ) AS published_at, \
                v.channel_id \
         FROM watch_history wh \
         JOIN videos v ON v.video_id = wh.video_id \
         LEFT JOIN channels ch ON ch.channel_id = v.channel_id \
         WHERE wh.child_account_id = ? \
         ORDER BY wh.last_watched_at DESC \
         LIMIT ?",
    )
    .bind(current.id)
    .bind(WATCH_AGAIN_LIMIT)
    .fetch_all(&state.db)
    .await?;

    filter_watch_history_rows(&state.db, current.id, rows, |r| {
        is_effectively_finished(r.progress_seconds, r.duration_seconds)
    })
    .await
}

/// Best-effort batched `video_id → channel_id` lookup for rows whose
/// `videos.channel_id` is still NULL. This happens when a stub
/// `videos` row was created (e.g. by `models::video::upsert_stub` from
/// a hand-allowlist add or imported-history import) before any
/// enriching upsert ran, and a separate writer has since landed a row
/// in `channel_videos` carrying the resolved `channel_id`. The hot
/// path (RSS-discovered videos) populates `videos.channel_id`
/// synchronously via `models::video::upsert`, so this fallback fires
/// only on those stub-first sequences. Reads from `channel_videos`,
/// the unified archive populated by RSS + sidecar + yt-dlp backfill.
/// Returns an empty map when there are no unresolved rows, or when
/// none of the unresolved videos have a `channel_videos` row to
/// resolve against.
///
/// **Lazy backfill**: when this lookup *does* resolve a channel_id,
/// we opportunistically write it back to `videos.channel_id` so the
/// next call sees a populated `v.channel_id` and skips this fallback
/// path entirely for the same `video_id`. This bounds the asymptotic
/// cost: even if a child has a large stub-only history, each row pays
/// this lookup at most once. The write-back failure is logged but
/// non-fatal — surfacing the resolved id to *this* request is the
/// primary contract; persisting it is a cache warm-up.
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
    // The query string only ever interpolates `?` placeholders (a
    // fixed alphabet of one character with no user input), so wrapping
    // in `AssertSqlSafe` is sound for sqlx 0.9's `SqlSafeStr` bound.
    let placeholders = std::iter::repeat_n("?", video_ids.len())
        .collect::<Vec<_>>()
        .join(",");
    // Aggregate cross-listed videos in SQL with `MAX(channel_id)` so
    // the resolved channel is deterministic. Without `GROUP BY` +
    // `MAX`, a video that appears under multiple channels in
    // `channel_videos` would return multiple rows here, and the
    // downstream `HashMap::entry(...).or_insert(...)` would pick a
    // hash-state-dependent channel — flipping request-to-request and
    // confusing the access check that consumes the map. We use
    // `MAX(channel_id)` to match migration 025 step 2's tie-break for
    // the parallel "rebuild `videos.channel_id` from
    // `channel_videos`" UPDATE, so the in-memory map and the on-disk
    // value converge to the same channel for the same video. The
    // `NOT NULL` guard preserves the existing semantics of skipping
    // rows that haven't resolved a channel yet.
    let sql = format!(
        "SELECT video_id, MAX(channel_id) AS channel_id \
           FROM channel_videos \
          WHERE video_id IN ({placeholders}) \
            AND channel_id IS NOT NULL \
          GROUP BY video_id"
    );
    let mut q = sqlx::query_as::<_, (String, String)>(sqlx::AssertSqlSafe(sql));
    for id in video_ids {
        q = q.bind(*id);
    }
    let rows = q.fetch_all(db).await?;
    // `GROUP BY video_id` already guarantees one row per video_id,
    // so `extend` is correct (no `or_insert` needed); the HashMap is
    // a convenient lookup shape for the caller, not a dedupe tool.
    let map: std::collections::HashMap<String, String> = rows.into_iter().collect();

    // Opportunistic write-back: persist each resolved channel_id onto
    // the canonical `videos` row so the next feed call sees it
    // populated and skips this fallback. The `WHERE channel_id IS
    // NULL` guard makes this idempotent and ensures we don't clobber
    // a value some other writer (heartbeat, RSS) raced ahead of us
    // to set. Failure here is best-effort: the caller already has the
    // map we computed above, so a transient DB issue at write-back
    // time degrades to "do this lookup again next call" rather than
    // a 500 on an otherwise-serviceable read.
    //
    // Implementation: a single chunked CTE+UPDATE statement rather
    // than N per-row UPDATEs. A cold call on a stub-only history
    // would otherwise be O(N) round-trips through the pool;
    // collapsing to one statement per chunk keeps the first-call
    // cost bounded by a single network / WAL roundtrip. We use the
    // `WITH src(vid, cid) AS (VALUES (?,?), …)` form because SQLite
    // does not accept the `… FROM (VALUES …) AS src(vid, cid)` shape
    // (no column-list aliases on subquery/VALUES table aliases) —
    // CTE column lists are the supported way to name VALUES columns.
    if !map.is_empty() {
        backfill_video_channel_ids(db, &map).await;
    }

    Ok(map)
}

/// Persist a batch of resolved `(video_id, channel_id)` pairs back to
/// `videos.channel_id` in a single chunked statement.
///
/// Separated from `lookup_channel_ids_for_videos` so the SQL building
/// and the chunking logic have a focused unit-testable surface, and
/// so future callers that resolve `channel_id` by other means (e.g.
/// a future sidecar) can reuse the same idempotent write-back path.
///
/// **Idempotency**: the `WHERE videos.channel_id IS NULL` guard makes
/// it safe to call this with stale data — a concurrent writer that
/// already populated the row wins and we no-op.
///
/// **Error handling**: best-effort. A transient failure (lock,
/// timeout) is `debug!`-logged because the caller has already computed
/// the in-memory map that the live request needs; the write-back
/// exists purely to amortise future lookups. A parse-error-shaped
/// failure escalates to `warn!`-once-per-process because that means
/// the helper is permanently broken (e.g. a future SQLite/syntax
/// regression) rather than a transient blip — exactly the case where
/// `debug!` alone hides the failure. The next call still re-tries
/// either way; the escalation is purely operator-visibility.
async fn backfill_video_channel_ids(
    db: &sqlx::SqlitePool,
    map: &std::collections::HashMap<String, String>,
) {
    use crate::services::sql_helpers;
    // Two binds per row (video_id, channel_id) — chunk so we never
    // exceed the bind-parameter ceiling even on pathologically large
    // maps. In practice `map.len()` is bounded by the caller's LIMIT
    // (≤ 50 today), so the loop body executes once; the chunking is
    // belt-and-braces for future callers that resolve larger batches.
    const COLS: usize = 2;
    let chunk_size = (sql_helpers::MAX_BIND_PARAMS / COLS).max(1);
    // Sort by (video_id, channel_id) so chunk boundaries are
    // deterministic across calls. The map's iteration order is
    // hash-state-dependent, which would make repeat calls (e.g. two
    // concurrent feed requests resolving the same set of stubs)
    // partition into different chunks — harmless for correctness
    // (each chunk's WHERE-IS-NULL guard is idempotent), but it
    // muddies debugging when an operator inspects which chunk
    // emitted a warn. The cross-listed-channel determinism that
    // matters for the *resolved channel id* is already enforced
    // upstream by the `MAX(channel_id)` aggregation in
    // `lookup_channel_ids_for_videos` (matches migration 025 step
    // 2's tie-break), so this sort is purely about chunk stability.
    let mut entries: Vec<(&String, &String)> = map.iter().collect();
    entries.sort_by(|a, b| a.0.cmp(b.0).then_with(|| a.1.cmp(b.1)));
    for chunk in entries.chunks(chunk_size) {
        // CTE-with-column-list form: `WITH src(vid, cid) AS
        // (VALUES (?,?), …) UPDATE videos SET channel_id = (SELECT
        // cid FROM src WHERE src.vid = videos.video_id) WHERE
        // videos.video_id IN (SELECT vid FROM src) AND
        // videos.channel_id IS NULL`. SQLite accepts CTE column
        // lists but rejects column-list aliases on subquery/VALUES
        // table aliases (a SQLite-wide limitation, not version-
        // dependent), so the correlated-subquery form is the
        // portable way to do a bulk UPDATE keyed by an inline table.
        let sql = format!(
            "WITH src(vid, cid) AS (VALUES {placeholders}) \
             UPDATE videos \
                SET channel_id = (SELECT cid FROM src WHERE src.vid = videos.video_id) \
              WHERE videos.video_id IN (SELECT vid FROM src) \
                AND videos.channel_id IS NULL",
            placeholders = sql_helpers::row_placeholders(chunk.len(), COLS)
        );
        let mut q = sqlx::query(sqlx::AssertSqlSafe(sql));
        for (video_id, channel_id) in chunk {
            // Bind order matches the `(vid, cid)` column list above.
            q = q.bind(*video_id).bind(*channel_id);
        }
        if let Err(err) = q.execute(db).await {
            let msg = err.to_string();
            // Heuristic: SQLite parse errors surface as "near ...:
            // syntax error" or "no such column" or "ambiguous
            // column". These are not transient; the helper will keep
            // returning the same error on every call until the SQL
            // is patched, so emit at `warn!` once (per process) so
            // it shows up in normal log streams rather than being
            // buried under `debug!`. Transient failures (busy,
            // locked, timeout) stay at `debug!` because every
            // subsequent call recovers and noisy warns would
            // overwhelm operators.
            let permanent = msg.contains("syntax error")
                || msg.contains("no such column")
                || msg.contains("no such table")
                || msg.contains("ambiguous column");
            if permanent {
                static WARNED: std::sync::atomic::AtomicBool =
                    std::sync::atomic::AtomicBool::new(false);
                if !WARNED.swap(true, std::sync::atomic::Ordering::Relaxed) {
                    tracing::warn!(
                        chunk_len = chunk.len(),
                        error = %err,
                        "lazy videos.channel_id backfill failed with a parse-shaped error; \
                         this indicates a permanent SQL/schema regression and will not \
                         self-recover. Subsequent failures will be debug!-logged to avoid \
                         flooding."
                    );
                }
            } else {
                tracing::debug!(
                    chunk_len = chunk.len(),
                    error = %err,
                    "lazy videos.channel_id backfill chunk failed; will retry on next feed call"
                );
            }
        }
    }
}

/// Whether a saved (`progress_seconds`, `duration_seconds`) pair should
/// be treated as "done" for continue-watching purposes. Rows with no
/// known duration are never auto-finished — we can't tell where the
/// end is, so we let the user remove them by re-watching.
///
/// `progress_seconds == 0` is treated as not-finished by design: a row
/// can land in `watch_history` with a zero (or never-incremented)
/// position via the heartbeat path before the player has emitted any
/// progress update. Promoting such rows to "finished" would surface
/// videos the child never actually watched.
fn is_effectively_finished(progress_seconds: i64, duration_seconds: Option<i64>) -> bool {
    let Some(duration) = duration_seconds else {
        return false;
    };
    if duration <= 0 || progress_seconds <= 0 {
        return false;
    }
    // Use the *later* of the two thresholds so neither rule fires
    // prematurely on the other's edge case. The tail rule alone would
    // mark a barely-started 20s clip as finished (15s tail ≥ 5s
    // played); the ratio rule alone would only kick in past 95% which
    // can leave a 2-minute video with a few seconds of unwatched
    // credits stuck in the row. Taking the max keeps long-form videos
    // bounded by the absolute tail and protects short clips with the
    // ratio.
    let tail_threshold = duration.saturating_sub(CONTINUE_TAIL_SECONDS);
    let ratio_threshold = ((duration as f64) * CONTINUE_COMPLETION_RATIO).ceil() as i64;
    progress_seconds >= tail_threshold.max(ratio_threshold)
}

#[derive(Debug, Serialize, Clone)]
pub struct NewVideoItem {
    pub video_id: String,
    pub title: String,
    pub channel_id: Option<String>,
    pub channel_title: Option<String>,
    pub thumbnail_url: Option<String>,
    /// Source publish date as unix seconds. `None` when the cache row
    /// never resolved a timestamp.
    pub published_at: Option<i64>,
    pub source_kind: String, // "channel"
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
        sidecar_fallback_dormant_interval_s: cfg.sidecar_fallback_dormant_interval.as_secs(),
        sidecar_fallback_archived_interval_s: cfg.sidecar_fallback_archived_interval.as_secs(),
        sidecar_dormant_threshold_days: cfg.sidecar_dormant_threshold_days,
        sidecar_archived_threshold_days: cfg.sidecar_archived_threshold_days,
        sidecar_fallback_max_per_hour: cfg.sidecar_fallback_max_per_hour,
        raw: RefresherSettingsRaw {
            dispatch_delay_ms: raw.dispatch_delay_ms,
            max_inflight: raw.max_inflight,
            batch_size: raw.batch_size,
            idle_tick_s: raw.idle_tick_s,
            channel_interval_s: raw.channel_interval_s,
            sidecar_fallback_enabled: raw.sidecar_fallback_enabled,
            sidecar_fallback_min_interval_s: raw.sidecar_fallback_min_interval_s,
            sidecar_fallback_dormant_interval_s: raw.sidecar_fallback_dormant_interval_s,
            sidecar_fallback_archived_interval_s: raw.sidecar_fallback_archived_interval_s,
            sidecar_dormant_threshold_days: raw.sidecar_dormant_threshold_days,
            sidecar_archived_threshold_days: raw.sidecar_archived_threshold_days,
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
    /// Per-source minimum interval (seconds) between sidecar fallbacks
    /// for an *active* channel (most recent upload within
    /// `sidecar_dormant_threshold_days`).
    pub sidecar_fallback_min_interval_s: u64,
    /// Per-source interval for a *dormant* channel (most recent upload
    /// 30-90d ago by default).
    pub sidecar_fallback_dormant_interval_s: u64,
    /// Per-source interval for an *archived* channel (no recent upload).
    pub sidecar_fallback_archived_interval_s: u64,
    /// Days since last upload before active → dormant.
    pub sidecar_dormant_threshold_days: u64,
    /// Days since last upload before dormant → archived.
    pub sidecar_archived_threshold_days: u64,
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
    pub sidecar_fallback_dormant_interval_s: Option<String>,
    pub sidecar_fallback_archived_interval_s: Option<String>,
    pub sidecar_dormant_threshold_days: Option<String>,
    pub sidecar_archived_threshold_days: Option<String>,
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
    pub sidecar_fallback_dormant_interval_s: Option<u64>,
    #[serde(default)]
    pub sidecar_fallback_archived_interval_s: Option<u64>,
    #[serde(default)]
    pub sidecar_dormant_threshold_days: Option<u64>,
    #[serde(default)]
    pub sidecar_archived_threshold_days: Option<u64>,
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
        KEY_MAX_INFLIGHT, KEY_SIDECAR_ARCHIVED_THRESHOLD_DAYS, KEY_SIDECAR_DORMANT_THRESHOLD_DAYS,
        KEY_SIDECAR_FALLBACK_ARCHIVED_INTERVAL_S, KEY_SIDECAR_FALLBACK_DORMANT_INTERVAL_S,
        KEY_SIDECAR_FALLBACK_ENABLED, KEY_SIDECAR_FALLBACK_MAX_PER_HOUR,
        KEY_SIDECAR_FALLBACK_MIN_INTERVAL_S, RANGE_BATCH_SIZE, RANGE_CHANNEL_INTERVAL_S,
        RANGE_DISPATCH_DELAY_MS, RANGE_IDLE_TICK_S, RANGE_MAX_INFLIGHT,
        RANGE_SIDECAR_FALLBACK_ARCHIVED_INTERVAL_S, RANGE_SIDECAR_FALLBACK_MAX_PER_HOUR,
        RANGE_SIDECAR_FALLBACK_MIN_INTERVAL_S, RANGE_SIDECAR_THRESHOLD_DAYS,
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
    if let Some(v) = body.sidecar_fallback_dormant_interval_s {
        if !RANGE_SIDECAR_FALLBACK_MIN_INTERVAL_S.contains(&v) {
            return Err(AppError::BadRequest(format!(
                "sidecar_fallback_dormant_interval_s must be {}..={}",
                RANGE_SIDECAR_FALLBACK_MIN_INTERVAL_S.start(),
                RANGE_SIDECAR_FALLBACK_MIN_INTERVAL_S.end()
            )));
        }
        set_config_value(
            &state.db,
            KEY_SIDECAR_FALLBACK_DORMANT_INTERVAL_S,
            &v.to_string(),
        )
        .await?;
    }
    if let Some(v) = body.sidecar_fallback_archived_interval_s {
        if !RANGE_SIDECAR_FALLBACK_ARCHIVED_INTERVAL_S.contains(&v) {
            return Err(AppError::BadRequest(format!(
                "sidecar_fallback_archived_interval_s must be {}..={}",
                RANGE_SIDECAR_FALLBACK_ARCHIVED_INTERVAL_S.start(),
                RANGE_SIDECAR_FALLBACK_ARCHIVED_INTERVAL_S.end()
            )));
        }
        set_config_value(
            &state.db,
            KEY_SIDECAR_FALLBACK_ARCHIVED_INTERVAL_S,
            &v.to_string(),
        )
        .await?;
    }
    if let Some(v) = body.sidecar_dormant_threshold_days {
        if !RANGE_SIDECAR_THRESHOLD_DAYS.contains(&v) {
            return Err(AppError::BadRequest(format!(
                "sidecar_dormant_threshold_days must be {}..={}",
                RANGE_SIDECAR_THRESHOLD_DAYS.start(),
                RANGE_SIDECAR_THRESHOLD_DAYS.end()
            )));
        }
        set_config_value(
            &state.db,
            KEY_SIDECAR_DORMANT_THRESHOLD_DAYS,
            &v.to_string(),
        )
        .await?;
    }
    if let Some(v) = body.sidecar_archived_threshold_days {
        if !RANGE_SIDECAR_THRESHOLD_DAYS.contains(&v) {
            return Err(AppError::BadRequest(format!(
                "sidecar_archived_threshold_days must be {}..={}",
                RANGE_SIDECAR_THRESHOLD_DAYS.start(),
                RANGE_SIDECAR_THRESHOLD_DAYS.end()
            )));
        }
        set_config_value(
            &state.db,
            KEY_SIDECAR_ARCHIVED_THRESHOLD_DAYS,
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

/// `GET /api/admin/channel-sync-state` — parent-only diagnostics.
///
/// Returns one row per tracked channel with its RSS poll bookkeeping
/// (rss_last_polled_at, rss_last_success_at, rss_last_error, …) plus
/// the backfill tier's bookkeeping (backfill_status, backfill_next_at,
/// …) plus live/tombstoned video counts. Surfaces freshness +
/// backfill health together so the operator can correlate issues
/// across tiers.
///
/// (Renamed from `/api/admin/feed-sources` when migration 020
/// consolidated `feed_sources` into `channel_sync_state`.)
pub async fn admin_list_sources(
    State(state): State<AppState>,
) -> AppResult<Json<Vec<feed_cache::ChannelSyncStateStatus>>> {
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
    /// Number of allowlisted-channel rows we currently track.
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
/// Reads from the `channel_videos` cache populated by the
/// [`crate::services::feed_refresher`] (RSS + sidecar) and
/// [`crate::services::channel_backfill`] (yt-dlp) background tasks. The handler
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
        (Some("channel"), Some(channel_id)) => {
            up_next_from_channel(&state, current.id, channel_id).await?
        }
        _ => up_next_from_new_videos(&state, current.id, limit).await?,
    };

    // Exclude videos the child has already watched so the queue
    // actually rotates instead of resurfacing the same items.
    let watched: HashSet<String> = child_watched_video_ids(&state.db, current.id)
        .await
        .unwrap_or_default();

    // Drop the current video, watched items, and
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

async fn up_next_from_channel(
    state: &AppState,
    child_id: i64,
    channel_id: &str,
) -> AppResult<Vec<UpNextItem>> {
    // Read from the local archive populated by RSS + sidecar + yt-dlp
    // backfill — no YouTube round-trip on the request path.
    let rows: Vec<UpNextRow> = sqlx::query_as(
        "SELECT cv.video_id, v.title, cv.channel_id, ch.channel_title, \
                v.thumbnail_url \
           FROM channel_videos cv \
           JOIN videos v ON v.video_id = cv.video_id \
           LEFT JOIN channels ch ON ch.channel_id = cv.channel_id \
          WHERE cv.channel_id = ? AND cv.is_deleted = 0 \
          ORDER BY COALESCE(cv.published_at, cv.last_seen_at) DESC, cv.last_seen_at DESC \
          LIMIT 25",
    )
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

/// One row pulled from `channel_videos` for the up-next builder.
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

    let mut buckets: Vec<Vec<UpNextItem>> = Vec::new();
    for channel_id in &channels {
        let rows: Vec<UpNextRow> = sqlx::query_as(
            "SELECT cv.video_id, v.title, cv.channel_id, ch.channel_title, \
                    v.thumbnail_url \
               FROM channel_videos cv \
               JOIN videos v ON v.video_id = cv.video_id \
               LEFT JOIN channels ch ON ch.channel_id = cv.channel_id \
              WHERE cv.channel_id = ? AND cv.is_deleted = 0 \
              ORDER BY COALESCE(cv.published_at, cv.last_seen_at) DESC, cv.last_seen_at DESC \
              LIMIT 5",
        )
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
