//! DB layer for the per-channel video archive and freshness sync state.
//!
//! Two tables back this module (see `migrations/020_channel_archive_sync.sql`):
//!
//! - `channels` — one row per channel currently allowlisted
//!   by any child. Carries RSS poll metadata (etag, last success, next
//!   scheduled poll), the sidecar fallback cooldown timestamp, the
//!   backfill loop's lease/status columns, and the channel header
//!   metadata (title, thumbnail, description) served by
//!   `GET /api/channels/:channelId`.
//! - `channel_videos` — the full archive of video stubs per channel,
//!   written to by RSS, the InnerTube sidecar fallback, and the yt-dlp
//!   `--flat-playlist` backfill. The `/api/feed/new-videos` handler
//!   reads from this table joined against the requesting child's
//!   allowlist.
//!
//! This module is pure DB; no networking, no scheduling decisions. The
//! [`crate::services::feed_refresher`] task drives RSS+sidecar polling
//! and calls [`upsert_channel_videos_from_rss`] /
//! [`upsert_channel_videos_from_sidecar`] / [`record_poll_success`] /
//! [`record_poll_failure`] here. The [`crate::services::channel_backfill`]
//! task drives the monthly yt-dlp backfill.

use sqlx::{SqliteExecutor, SqlitePool};

use crate::error::AppResult;
use crate::routes::feed::NewVideoItem;
use crate::services::sql_helpers;

/// Conceptual kind value emitted in API responses for any item served
/// out of `channel_videos`. The schema no longer carries a kind column
/// (migration 017 already locked it to 'channel'; migration 020
/// consolidated the table away), but `NewVideoItem.source_kind` is
/// still part of the public API response shape.
pub const KIND_CHANNEL: &str = "channel";

/// One item destined for `channel_videos`. Built by the source-specific
/// fetchers (RSS, sidecar) and handed to the upsert functions in a
/// batch.
#[derive(Debug, Clone)]
pub struct ItemRow {
    pub video_id: String,
    pub title: String,
    pub channel_id: Option<String>,
    pub channel_title: Option<String>,
    pub thumbnail_url: Option<String>,
    /// Unix seconds; `None` if the source didn't provide a published
    /// timestamp (rare).
    pub published_at: Option<i64>,
    /// Original ISO-8601 string echoed back in the API response.
    pub published_raw: Option<String>,
}

/// One row from `channels`, used by the refresher when
/// picking work.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct DueSource {
    pub channel_id: String,
    pub rss_etag: Option<String>,
    pub rss_last_modified: Option<String>,
    pub rss_consecutive_errors: i64,
    /// Unix-seconds timestamp of the most recent sidecar fallback for
    /// this channel (`NULL` if none has ever happened). Used by the
    /// refresher to enforce the per-channel rate cap without a second
    /// query.
    pub last_sidecar_fallback_at: Option<i64>,
}

/// Statistics returned by the upsert functions for diagnostics.
#[derive(Debug, Clone, Default)]
pub struct UpsertStats {
    pub inserted: u64,
    pub updated: u64,
    pub untombstoned: u64,
}

/// One row of `channels` for the admin diagnostics endpoint.
/// Renamed from `FeedSourceStatus` to reflect the consolidated table.
#[derive(Debug, Clone, sqlx::FromRow, serde::Serialize)]
pub struct ChannelSyncStateStatus {
    pub channel_id: String,
    pub channel_title: Option<String>,
    pub rss_last_polled_at: Option<i64>,
    pub rss_last_success_at: Option<i64>,
    pub rss_last_error: Option<String>,
    pub rss_consecutive_errors: i64,
    pub rss_next_poll_at: i64,
    /// Live videos for the channel (`is_deleted = 0`).
    pub item_count: i64,
    /// Tombstoned videos for the channel (`is_deleted = 1`), surfaced
    /// for the diagnostics UI's "archived (channel removed)" column.
    pub archived_count: i64,
    /// Unix-seconds timestamp of the most recent sidecar fallback for
    /// this channel, or `NULL` if none has ever happened. Surfaced so
    /// the diagnostics UI can show "last fallback: 5m ago" and the
    /// operator can correlate sidecar load with RSS outage windows.
    pub last_sidecar_fallback_at: Option<i64>,
    /// Backfill tier status: pending / running / complete / failed / shelved.
    pub backfill_status: String,
    pub backfill_last_completed_at: Option<i64>,
    pub backfill_last_error: Option<String>,
    pub backfill_consecutive_errors: i64,
    pub backfill_next_at: i64,
}

/// Insert a `channel_id` row if missing. Sets `rss_next_poll_at = 0`
/// and `backfill_next_at = 0` so both background loops pick it up on
/// their next tick. Idempotent.
pub async fn upsert_channel(pool: &SqlitePool, channel_id: &str) -> AppResult<()> {
    sqlx::query(
        "INSERT INTO channels \
             (channel_id, backfill_status, backfill_next_at, rss_next_poll_at) \
         VALUES (?, 'pending', 0, 0) \
         ON CONFLICT(channel_id) DO NOTHING",
    )
    .bind(channel_id)
    .execute(pool)
    .await?;
    Ok(())
}

/// Insert a `channel_id` row with explicit header metadata, or update
/// any existing row's metadata fields. Used by the allowlist POST
/// handler so the channel page can render title/thumbnail/description
/// from local state without a sidecar call on every visit, and by the
/// RSS / sidecar / backfill paths so YouTube renames propagate.
///
/// **Rename propagation:** the conflict path lets a non-null `excluded`
/// value win over the stored value. Callers that don't have a fresh
/// title pass `None` (or filter empty strings to `None`); a `None`
/// arrives as SQL `NULL` and `COALESCE(NULLIF(excluded, ''), stored)`
/// preserves the stored value. Net effect: any caller carrying a real
/// title overwrites the stored one (so YouTube renames land), and
/// callers without one don't clobber what's there.
pub async fn upsert_channel_with_metadata<'e, E>(
    exec: E,
    channel_id: &str,
    title: Option<&str>,
    thumbnail_url: Option<&str>,
    description: Option<&str>,
) -> AppResult<()>
where
    E: SqliteExecutor<'e>,
{
    sqlx::query(
        "INSERT INTO channels \
             (channel_id, channel_title, channel_thumbnail_url, description, \
              backfill_status, backfill_next_at, rss_next_poll_at) \
         VALUES (?, ?, ?, ?, 'pending', 0, 0) \
         ON CONFLICT(channel_id) DO UPDATE SET \
             channel_title         = COALESCE(NULLIF(excluded.channel_title, ''),         channels.channel_title), \
             channel_thumbnail_url = COALESCE(NULLIF(excluded.channel_thumbnail_url, ''), channels.channel_thumbnail_url), \
             description           = COALESCE(NULLIF(excluded.description, ''),           channels.description)",
    )
    .bind(channel_id)
    .bind(title)
    .bind(thumbnail_url)
    .bind(description)
    .execute(exec)
    .await?;
    Ok(())
}

/// Delete any `channels` rows whose `channel_id` no longer
/// appears in `allowlisted_channels`. The corresponding `channel_videos`
/// rows are cleaned up in the same transaction. Returns the number of
/// channels removed.
pub async fn gc_orphan_sources(pool: &SqlitePool) -> AppResult<u64> {
    let mut tx = pool.begin().await?;
    sqlx::query(
        "DELETE FROM channel_videos \
         WHERE channel_id NOT IN (SELECT channel_id FROM allowlisted_channels)",
    )
    .execute(&mut *tx)
    .await?;
    let result = sqlx::query(
        "DELETE FROM channels \
         WHERE channel_id NOT IN (SELECT channel_id FROM allowlisted_channels)",
    )
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(result.rows_affected())
}

/// Upsert RSS-fed items into `channel_videos`. Does NOT delete absent
/// rows (RSS only sees ~15 newest items; absence is not evidence of
/// deletion). Clears `is_deleted=0` on re-sighting. Does NOT touch
/// `duration_s` / `view_count` — RSS doesn't carry them and clobbering
/// with NULL would lose backfill-supplied data.
pub async fn upsert_channel_videos_from_rss(
    pool: &SqlitePool,
    channel_id: &str,
    items: &[ItemRow],
    now: i64,
) -> AppResult<UpsertStats> {
    upsert_channel_videos(pool, channel_id, items, now, "rss").await
}

/// Same as [`upsert_channel_videos_from_rss`] but tags `source='sidecar'`.
/// Used by the InnerTube sidecar fallback path in the refresher.
pub async fn upsert_channel_videos_from_sidecar(
    pool: &SqlitePool,
    channel_id: &str,
    items: &[ItemRow],
    now: i64,
) -> AppResult<UpsertStats> {
    upsert_channel_videos(pool, channel_id, items, now, "sidecar").await
}

/// Shared implementation for the RSS/sidecar upsert path. The
/// per-source variant just supplies the `source` tag.
///
/// **Round-trip footprint**: 1 batched SELECT + N UPSERTs in a single
/// transaction. The freshness path's batches are small by construction
/// (~15 items per RSS poll, ~30 per sidecar fallback), so this is
/// cheap. The per-item SELECT-then-UPSERT loop in earlier versions
/// was 2N round-trips; the upfront `HashMap` lookup collapses it to
/// 1 + N. The same optimisation was applied to
/// [`crate::services::channel_backfill::apply_backfill_entries`]
/// where N can be 10k+ and the gain is dramatic; here it's modest
/// but consistent.
async fn upsert_channel_videos(
    pool: &SqlitePool,
    channel_id: &str,
    items: &[ItemRow],
    now: i64,
    source: &str,
) -> AppResult<UpsertStats> {
    use std::collections::HashMap;

    let mut tx = pool.begin().await?;
    let mut stats = UpsertStats::default();

    // Pre-fetch prior is_deleted state for every video_id in the
    // batch in one query, so the per-item branch is an O(1) HashMap
    // lookup rather than an extra round-trip to SQLite per item.
    let existing: HashMap<String, i64> = if items.is_empty() {
        HashMap::new()
    } else {
        // Freshness batches are small by construction (RSS≈15,
        // sidecar≈30), well below `sql_helpers::MAX_BIND_PARAMS`. We
        // therefore don't chunk — but we *do* defensively assert
        // against the ceiling so a future caller bumping batch size
        // (e.g., a sidecar full-page pull) gets a loud debug-build
        // failure instead of a runtime "too many SQL variables".
        // Total bind count is `items.len() + 1` (the IN-clause binds
        // plus the leading `channel_id` bind). The comparison form
        // `items.len() < MAX_BIND_PARAMS` is equivalent to
        // `items.len() + 1 <= MAX_BIND_PARAMS` (integer arithmetic):
        // both trip when `items.len() == MAX_BIND_PARAMS`, i.e. when
        // the total bind count would be `MAX_BIND_PARAMS + 1`. We
        // prefer the `<` form because clippy's `int_plus_one` lint
        // bans the `+ 1 <=` shape, and the message below makes the
        // accounting explicit so a future maintainer doesn't have
        // to re-derive the equivalence.
        debug_assert!(
            items.len() < sql_helpers::MAX_BIND_PARAMS,
            "freshness batch (items={} + 1 channel_id bind = {} total binds) exceeds MAX_BIND_PARAMS={}",
            items.len(),
            items.len() + 1,
            sql_helpers::MAX_BIND_PARAMS,
        );
        // The IN-clause placeholders share the same `AssertSqlSafe`
        // audit surface as `sql_helpers::row_placeholders`: only `?`
        // and `,` ever appear, no caller-provided strings. The shape
        // here is a flat `(?,?,?)` (not `(?),(?)`), so we build it
        // inline rather than via `row_placeholders` which is tuned
        // for VALUES-style INSERTs.
        let placeholders = std::iter::repeat_n("?", items.len())
            .collect::<Vec<_>>()
            .join(",");
        let sql = format!(
            "SELECT video_id, is_deleted FROM channel_videos \
              WHERE channel_id = ? AND video_id IN ({placeholders})"
        );
        let mut q = sqlx::query_as::<_, (String, i64)>(sqlx::AssertSqlSafe(sql)).bind(channel_id);
        for item in items {
            q = q.bind(&item.video_id);
        }
        q.fetch_all(&mut *tx).await?.into_iter().collect()
    };

    // INVARIANT: the parent `channels` row exists by the time we get
    // here. Every code path that triggers this function (allowlist
    // POST, RSS refresher's `claim_due_sources` SELECT, channel
    // backfill loop, GC reseed) seeds the `channels` row before
    // queuing work for it. The FK on `channel_videos.channel_id`
    // (added in migration 025) is the production canary — a violation
    // surfaces as a hard INSERT failure rather than silent data drift.

    // Propagate `channels.channel_title` once per RSS poll, not once
    // per item. YouTube emits the channel title on every entry of a
    // feed (they're all the same value), so without this hoist a
    // 25-item feed issued 25 identical UPDATEs against the same row.
    //
    // **First-wins** by design: `find_map` returns on the first non-
    // blank `channel_title` it sees. In practice every item in a
    // single feed batch carries the *same* title (YouTube emits the
    // channel name verbatim on every `<entry>`), so "first" and
    // "most recent" coincide. The one edge case where they diverge
    // is a channel rename observed mid-page-pull (two batches from
    // YouTube with a rename event in between); in that case
    // whichever batch the refresher polls last wins the persisted
    // value, and first-wins-within-a-batch is correct because
    // YouTube's batch is internally consistent.
    //
    // Skip-when-equal: poll-to-poll the title is almost always
    // unchanged, and SQLite's UPDATE still produces a WAL frame even
    // when no column value changes. `WHERE channel_title IS NOT ?`
    // uses SQLite's null-safe `IS NOT` comparator so a stored NULL vs
    // a non-null candidate is also caught, and the UPDATE silently
    // affects zero rows (and writes no WAL frame) on a match. This
    // collapses the previous SELECT-then-conditional-UPDATE into a
    // single statement with the same I/O profile on the steady-state
    // path and one fewer round-trip on the rename path.
    let feed_channel_title = items
        .iter()
        .find_map(|i| i.channel_title.as_deref().filter(|s| !s.is_empty()));
    if let Some(ct) = feed_channel_title {
        // Propagate the error rather than swallowing it: the
        // invariant above guarantees the row exists, so any failure
        // here (deadlock, broken transaction) is a real bug we want
        // surfaced rather than masked by a later opaque "transaction
        // not active" error from the next statement.
        sqlx::query(
            "UPDATE channels SET channel_title = ? \
              WHERE channel_id = ? AND channel_title IS NOT ?",
        )
        .bind(ct)
        .bind(channel_id)
        .bind(ct)
        .execute(&mut *tx)
        .await?;
    }

    for item in items {
        // Upsert the canonical `videos` row first. `channel_id` /
        // thumbnail are refreshed via NULLIF inside the helper so the
        // RSS-supplied data overrides earlier sightings when present.
        // Filter empty strings to None to uphold the
        // `models::video::upsert` caller contract (see the
        // "INSERT-side NULLIF" comment in src/models/video.rs). The
        // INSERT path doesn't wrap binds in NULLIF, so a `Some("")`
        // would persist an empty thumbnail on the first sighting of a
        // new video — harmless (UI COALESCEs back to blank) but
        // contract-violating.
        crate::models::video::upsert(
            &mut *tx,
            &item.video_id,
            Some(item.title.as_str()).filter(|s| !s.is_empty()),
            item.channel_id.as_deref().or(Some(channel_id)),
            None,
            item.thumbnail_url.as_deref().filter(|s| !s.is_empty()),
        )
        .await?;

        sqlx::query(
            "INSERT INTO channel_videos \
                 (channel_id, video_id, published_at, published_raw, \
                  first_seen_at, last_seen_at, source, is_deleted) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?5, ?6, 0) \
             ON CONFLICT(channel_id, video_id) DO UPDATE SET \
                 published_at  = COALESCE(channel_videos.published_at, excluded.published_at), \
                 published_raw = COALESCE(channel_videos.published_raw, excluded.published_raw), \
                 last_seen_at  = excluded.last_seen_at, \
                 source        = excluded.source, \
                 is_deleted    = 0",
        )
        .bind(channel_id)
        .bind(&item.video_id)
        .bind(item.published_at)
        .bind(&item.published_raw)
        .bind(now)
        .bind(source)
        .execute(&mut *tx)
        .await?;

        match existing.get(&item.video_id) {
            None => stats.inserted += 1,
            Some(&prior_is_deleted) if prior_is_deleted != 0 => stats.untombstoned += 1,
            Some(_) => stats.updated += 1,
        }
    }

    tx.commit().await?;
    Ok(stats)
}

/// Inputs to [`record_poll_success`]. Bundled into a struct so the
/// (otherwise 6-positional-arg) call site stays readable.
#[derive(Debug)]
pub struct PollSuccess<'a> {
    pub channel_id: &'a str,
    pub title: Option<&'a str>,
    pub etag: Option<&'a str>,
    pub last_modified: Option<&'a str>,
    pub next_poll_at: i64,
    pub now: i64,
}

/// Mark a poll as successful. Caches the new etag/last-modified pair so
/// the next poll can issue a conditional GET, resets the error counter,
/// updates the title (if the source returned one), and schedules the
/// next poll.
pub async fn record_poll_success(pool: &SqlitePool, args: PollSuccess<'_>) -> AppResult<()> {
    let PollSuccess {
        channel_id,
        title,
        etag,
        last_modified,
        next_poll_at,
        now,
    } = args;
    sqlx::query(
        "UPDATE channels SET \
             channel_title          = COALESCE(NULLIF(?, ''), channel_title), \
             rss_etag               = ?, \
             rss_last_modified      = ?, \
             rss_last_polled_at     = ?, \
             rss_last_success_at    = ?, \
             rss_last_error         = NULL, \
             rss_consecutive_errors = 0, \
             rss_next_poll_at       = ? \
         WHERE channel_id = ?",
    )
    .bind(title)
    .bind(etag)
    .bind(last_modified)
    .bind(now)
    .bind(now)
    .bind(next_poll_at)
    .bind(channel_id)
    .execute(pool)
    .await?;
    Ok(())
}

/// Mark a poll as **deferred-by-policy** — no network call attempted,
/// row rescheduled, **and** `rss_consecutive_errors` left untouched.
/// Used when the refresher's rate caps temporarily deny a fallback for
/// an otherwise-eligible source: the source isn't in error and shouldn't
/// reset any existing error count from prior real failures, it just has
/// to wait its turn.
pub async fn record_poll_deferred(
    pool: &SqlitePool,
    channel_id: &str,
    reason: &str,
    next_poll_at: i64,
    now: i64,
) -> AppResult<()> {
    sqlx::query(
        "UPDATE channels SET \
             rss_last_polled_at = ?, \
             rss_last_error     = ?, \
             rss_next_poll_at   = ? \
         WHERE channel_id = ?",
    )
    .bind(now)
    .bind(reason)
    .bind(next_poll_at)
    .bind(channel_id)
    .execute(pool)
    .await?;
    Ok(())
}

/// Mark a poll as **skipped** — no network I/O happened, but the row
/// is rescheduled into the future and tagged so the diagnostics UI
/// can distinguish skipped-by-policy from a recent successful poll.
/// Resets `rss_consecutive_errors` so rows that previously accumulated
/// errors stop showing a misleading count.
pub async fn record_poll_skipped(
    pool: &SqlitePool,
    channel_id: &str,
    reason: &str,
    next_poll_at: i64,
) -> AppResult<()> {
    sqlx::query(
        "UPDATE channels SET \
             rss_last_error         = ?, \
             rss_consecutive_errors = 0, \
             rss_next_poll_at       = ? \
         WHERE channel_id = ?",
    )
    .bind(reason)
    .bind(next_poll_at)
    .bind(channel_id)
    .execute(pool)
    .await?;
    Ok(())
}

/// Mark a poll as failed. Increments `rss_consecutive_errors` and
/// schedules the next attempt. Does **not** clear cached items so the
/// feed continues to serve stale data through transient outages.
pub async fn record_poll_failure(
    pool: &SqlitePool,
    channel_id: &str,
    err: &str,
    next_poll_at: i64,
    now: i64,
) -> AppResult<()> {
    sqlx::query(
        "UPDATE channels SET \
             rss_last_polled_at     = ?, \
             rss_last_error         = ?, \
             rss_consecutive_errors = rss_consecutive_errors + 1, \
             rss_next_poll_at       = ? \
         WHERE channel_id = ?",
    )
    .bind(now)
    .bind(err)
    .bind(next_poll_at)
    .bind(channel_id)
    .execute(pool)
    .await?;
    Ok(())
}

/// Mark the source as confidently dead: the sidecar returned a clean
/// "channel not found" / 404. Pushes `rss_next_poll_at` far into the
/// future so the scheduler effectively shelves the row, clears the
/// error counter, and sets `rss_last_error` to a human-readable reason
/// for the diagnostics UI.
pub async fn record_source_dead(
    pool: &SqlitePool,
    channel_id: &str,
    reason: &str,
    next_poll_at: i64,
    now: i64,
) -> AppResult<()> {
    sqlx::query(
        "UPDATE channels SET \
             rss_last_polled_at     = ?, \
             rss_last_error         = ?, \
             rss_consecutive_errors = 0, \
             rss_next_poll_at       = ? \
         WHERE channel_id = ?",
    )
    .bind(now)
    .bind(reason)
    .bind(next_poll_at)
    .bind(channel_id)
    .execute(pool)
    .await?;
    Ok(())
}

/// Record that a sidecar fallback was dispatched for this channel.
/// Called *before* the sidecar request goes out so concurrent claims
/// (in the rare burst case) see the reservation and respect the
/// per-source cap.
pub async fn record_sidecar_fallback_dispatched(
    pool: &SqlitePool,
    channel_id: &str,
    now: i64,
) -> AppResult<()> {
    sqlx::query(
        "UPDATE channels SET last_sidecar_fallback_at = ? \
         WHERE channel_id = ?",
    )
    .bind(now)
    .bind(channel_id)
    .execute(pool)
    .await?;
    Ok(())
}

/// Count sidecar fallbacks dispatched in the last hour, used by the
/// refresher to enforce the aggregate per-hour cap.
pub async fn sidecar_fallbacks_in_last_hour(pool: &SqlitePool, now: i64) -> AppResult<i64> {
    let cutoff = now - 3600;
    let n: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM channels \
         WHERE last_sidecar_fallback_at IS NOT NULL \
           AND last_sidecar_fallback_at >= ?",
    )
    .bind(cutoff)
    .fetch_one(pool)
    .await?;
    Ok(n)
}

/// Atomically claim up to `limit` channels whose `rss_next_poll_at <= now`,
/// pushing their `rss_next_poll_at` forward by `lease_secs` so a
/// concurrent caller (or the next iteration of the refresher loop)
/// does not pick them up while the poll is in flight.
pub async fn claim_due_sources(
    pool: &SqlitePool,
    now: i64,
    limit: i64,
    lease_secs: i64,
) -> AppResult<Vec<DueSource>> {
    let leased_until = now.saturating_add(lease_secs);
    let rows = sqlx::query_as::<_, DueSource>(
        "UPDATE channels SET rss_next_poll_at = ? \
          WHERE channel_id IN ( \
              SELECT channel_id FROM channels \
               WHERE rss_next_poll_at <= ? \
               ORDER BY rss_next_poll_at ASC \
               LIMIT ? \
          ) \
          RETURNING channel_id, rss_etag, rss_last_modified, \
                    rss_consecutive_errors, last_sidecar_fallback_at",
    )
    .bind(leased_until)
    .bind(now)
    .bind(limit)
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

/// Build the new-videos feed for a child. Joins `channel_videos`
/// against the child's allowlist, excludes blocked + hidden videos
/// and tombstoned rows, dedupes by `video_id` keeping the most recent
/// `published_at`, and sorts/limits.
pub async fn feed_for_child(
    pool: &SqlitePool,
    child_id: i64,
    limit: usize,
) -> AppResult<Vec<NewVideoItem>> {
    #[derive(sqlx::FromRow)]
    struct Row {
        video_id: String,
        title: String,
        channel_id: String,
        channel_title: Option<String>,
        thumbnail_url: Option<String>,
        #[allow(dead_code)]
        published_raw: Option<String>,
        published_at: Option<i64>,
    }

    let fetch_limit = limit as i64;

    let rows: Vec<Row> = sqlx::query_as(
        "WITH allowed(channel_id) AS ( \
             SELECT channel_id FROM allowlisted_channels WHERE child_account_id = ?1 \
         ), \
         candidates AS ( \
             SELECT cv.video_id, v.title, cv.channel_id, ch.channel_title, \
                    v.thumbnail_url, cv.published_raw, cv.published_at, \
                    cv.last_seen_at, \
                    ROW_NUMBER() OVER ( \
                        PARTITION BY cv.video_id \
                        ORDER BY COALESCE(cv.published_at, cv.last_seen_at) DESC, cv.last_seen_at DESC \
                    ) AS rn \
               FROM channel_videos cv \
               JOIN videos v ON v.video_id = cv.video_id \
               LEFT JOIN channels ch ON ch.channel_id = cv.channel_id \
               JOIN allowed a ON a.channel_id = cv.channel_id \
              WHERE cv.is_deleted = 0 \
                AND NOT EXISTS ( \
                    SELECT 1 FROM blocked_videos b \
                     WHERE b.child_account_id = ?1 AND b.video_id = cv.video_id) \
                AND NOT EXISTS ( \
                    SELECT 1 FROM hidden_videos h \
                     WHERE h.child_account_id = ?1 AND h.video_id = cv.video_id) \
         ) \
         SELECT video_id, title, channel_id, channel_title, \
                thumbnail_url, published_raw, published_at \
           FROM candidates WHERE rn = 1 \
          ORDER BY COALESCE(published_at, last_seen_at) DESC, last_seen_at DESC \
          LIMIT ?2",
    )
    .bind(child_id)
    .bind(fetch_limit)
    .fetch_all(pool)
    .await?;

    let mut out = Vec::with_capacity(limit);
    for r in rows {
        out.push(NewVideoItem {
            video_id: r.video_id,
            title: r.title,
            channel_id: Some(r.channel_id.clone()),
            channel_title: r.channel_title,
            thumbnail_url: r.thumbnail_url,
            published_at: r.published_at,
            source_kind: KIND_CHANNEL.to_string(),
            source_id: r.channel_id,
        });
    }
    Ok(out)
}

/// Capacity-utilisation snapshot, used by the diagnostics endpoint
/// (and the parent settings UI) to surface "are we keeping up?" signal
/// in a single round-trip.
#[derive(Debug, Clone, sqlx::FromRow, serde::Serialize)]
pub struct FeedRefresherCapacityCounts {
    /// Total rows in `channels`.
    pub total_sources: i64,
    /// Sources whose `rss_next_poll_at <= now` *right now*.
    pub queue_depth: i64,
    /// Sources whose `rss_last_polled_at` falls inside the last hour.
    pub polls_last_hour: i64,
    /// Sidecar fallbacks dispatched in the last hour.
    pub sidecar_fallbacks_last_hour: i64,
}

/// Compute the per-table capacity counts. All four counts come from
/// a single query plan against `channels`.
pub async fn capacity_counts(
    pool: &SqlitePool,
    now: i64,
) -> AppResult<FeedRefresherCapacityCounts> {
    let hour_ago = now - 3600;
    let row: FeedRefresherCapacityCounts = sqlx::query_as(
        "SELECT \
             COUNT(*) AS total_sources, \
             COALESCE(SUM(CASE WHEN rss_next_poll_at <= ? THEN 1 ELSE 0 END), 0) \
                 AS queue_depth, \
             COALESCE(SUM(CASE WHEN rss_last_polled_at >= ? THEN 1 ELSE 0 END), 0) \
                 AS polls_last_hour, \
             COALESCE(SUM(CASE WHEN last_sidecar_fallback_at >= ? THEN 1 ELSE 0 END), 0) \
                 AS sidecar_fallbacks_last_hour \
           FROM channels",
    )
    .bind(now)
    .bind(hour_ago)
    .bind(hour_ago)
    .fetch_one(pool)
    .await?;
    Ok(row)
}

/// Diagnostic snapshot of every cached channel. Used by the admin
/// endpoint to surface poll + backfill health.
///
/// Uses a single LEFT JOIN against an aggregated subquery rather than
/// a per-row correlated subquery so the cost is O(N + M) instead of
/// O(N × index seeks). `item_count` and `archived_count` come from
/// the same conditional aggregation.
pub async fn list_source_status(pool: &SqlitePool) -> AppResult<Vec<ChannelSyncStateStatus>> {
    let rows = sqlx::query_as::<_, ChannelSyncStateStatus>(
        "SELECT s.channel_id, s.channel_title, \
                s.rss_last_polled_at, s.rss_last_success_at, s.rss_last_error, \
                s.rss_consecutive_errors, s.rss_next_poll_at, \
                COALESCE(c.item_count, 0) AS item_count, \
                COALESCE(c.archived_count, 0) AS archived_count, \
                s.last_sidecar_fallback_at, \
                s.backfill_status, s.backfill_last_completed_at, \
                s.backfill_last_error, s.backfill_consecutive_errors, \
                s.backfill_next_at \
           FROM channels s \
           LEFT JOIN ( \
                SELECT channel_id, \
                       SUM(CASE WHEN is_deleted = 0 THEN 1 ELSE 0 END) AS item_count, \
                       SUM(CASE WHEN is_deleted = 1 THEN 1 ELSE 0 END) AS archived_count \
                  FROM channel_videos \
                 GROUP BY channel_id \
           ) c ON c.channel_id = s.channel_id \
          ORDER BY s.channel_id ASC",
    )
    .fetch_all(pool)
    .await?;
    Ok(rows)
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

    async fn insert_child(pool: &SqlitePool, name: &str) -> i64 {
        sqlx::query(
            "INSERT INTO accounts (display_name, account_type, pin_hash, created_at, updated_at) \
             VALUES (?, 'child', 'x', unixepoch(), unixepoch())",
        )
        .bind(name)
        .execute(pool)
        .await
        .unwrap();
        sqlx::query_scalar::<_, i64>("SELECT last_insert_rowid()")
            .fetch_one(pool)
            .await
            .unwrap()
    }

    async fn allow_channel(pool: &SqlitePool, child: i64, channel_id: &str) {
        // Seed the parent `channels` row first (FK).
        upsert_channel_with_metadata(pool, channel_id, Some("X"), None, None)
            .await
            .unwrap();
        sqlx::query(
            "INSERT INTO allowlisted_channels (child_account_id, channel_id, added_by) \
             VALUES (?, ?, ?)",
        )
        .bind(child)
        .bind(channel_id)
        .bind(child)
        .execute(pool)
        .await
        .unwrap();
    }

    fn mk_item(video_id: &str, pub_at: i64) -> ItemRow {
        ItemRow {
            video_id: video_id.into(),
            title: format!("title-{video_id}"),
            channel_id: Some("UC1".into()),
            channel_title: Some("Channel One".into()),
            thumbnail_url: Some("https://t/x.jpg".into()),
            published_at: Some(pub_at),
            published_raw: Some(format!("2024-01-01T00:00:{pub_at:02}Z")),
        }
    }

    #[tokio::test]
    async fn upsert_and_gc() {
        let pool = setup_db().await;
        upsert_channel(&pool, "UC1").await.unwrap();
        upsert_channel(&pool, "UC1").await.unwrap();
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM channels")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(count, 1);

        // No allowlist row → gc removes it.
        let removed = gc_orphan_sources(&pool).await.unwrap();
        assert_eq!(removed, 1);
    }

    #[tokio::test]
    async fn rss_upsert_inserts_and_updates() {
        let pool = setup_db().await;
        upsert_channel(&pool, "UC1").await.unwrap();

        let items = vec![mk_item("vA", 100), mk_item("vB", 200)];
        let stats = upsert_channel_videos_from_rss(&pool, "UC1", &items, 9999)
            .await
            .unwrap();
        assert_eq!(stats.inserted, 2);
        assert_eq!(stats.updated, 0);

        // Second pass: same items → all updates, no new inserts. The
        // last_seen_at bumps; first_seen_at is preserved.
        let stats = upsert_channel_videos_from_rss(&pool, "UC1", &items, 10_500)
            .await
            .unwrap();
        assert_eq!(stats.inserted, 0);
        assert_eq!(stats.updated, 2);

        let (first, last): (i64, i64) = sqlx::query_as(
            "SELECT first_seen_at, last_seen_at FROM channel_videos \
              WHERE channel_id = 'UC1' AND video_id = 'vA'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(first, 9999);
        assert_eq!(last, 10_500);
    }

    #[tokio::test]
    async fn rss_upsert_clears_tombstone() {
        let pool = setup_db().await;
        upsert_channel(&pool, "UC1").await.unwrap();
        // Seed a tombstoned row.
        crate::models::video::upsert_stub(&pool, "vTomb")
            .await
            .unwrap();
        sqlx::query(
            "INSERT INTO channel_videos \
                 (channel_id, video_id, first_seen_at, last_seen_at, source, is_deleted) \
             VALUES ('UC1', 'vTomb', 100, 100, 'backfill', 1)",
        )
        .execute(&pool)
        .await
        .unwrap();

        let items = vec![mk_item("vTomb", 999)];
        let stats = upsert_channel_videos_from_rss(&pool, "UC1", &items, 200)
            .await
            .unwrap();
        assert_eq!(stats.untombstoned, 1);

        let is_deleted: i64 = sqlx::query_scalar(
            "SELECT is_deleted FROM channel_videos \
              WHERE channel_id = 'UC1' AND video_id = 'vTomb'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(is_deleted, 0, "RSS sighting must clear the tombstone");
    }

    #[tokio::test]
    async fn feed_for_child_respects_allowlist_and_blocks() {
        let pool = setup_db().await;
        let child = insert_child(&pool, "kid").await;

        upsert_channel(&pool, "UC1").await.unwrap();
        upsert_channel(&pool, "UC2").await.unwrap();
        upsert_channel_videos_from_rss(&pool, "UC1", &[mk_item("vA", 100), mk_item("vB", 200)], 0)
            .await
            .unwrap();
        upsert_channel_videos_from_rss(&pool, "UC2", &[mk_item("vC", 300)], 0)
            .await
            .unwrap();

        // Child only allowlists UC1.
        allow_channel(&pool, child, "UC1").await;

        let feed = feed_for_child(&pool, child, 10).await.unwrap();
        let ids: Vec<&str> = feed.iter().map(|i| i.video_id.as_str()).collect();
        assert_eq!(ids, vec!["vB", "vA"]);

        // Block vB; it drops out.
        sqlx::query(
            "INSERT INTO blocked_videos (child_account_id, video_id, blocked_by) \
             VALUES (?, 'vB', ?)",
        )
        .bind(child)
        .bind(child)
        .execute(&pool)
        .await
        .unwrap();

        let feed = feed_for_child(&pool, child, 10).await.unwrap();
        let ids: Vec<&str> = feed.iter().map(|i| i.video_id.as_str()).collect();
        assert_eq!(ids, vec!["vA"]);
    }

    #[tokio::test]
    async fn feed_for_child_excludes_tombstoned() {
        let pool = setup_db().await;
        let child = insert_child(&pool, "kid").await;
        upsert_channel(&pool, "UC1").await.unwrap();
        upsert_channel_videos_from_rss(&pool, "UC1", &[mk_item("vA", 100), mk_item("vB", 200)], 0)
            .await
            .unwrap();
        allow_channel(&pool, child, "UC1").await;

        // Tombstone vB.
        sqlx::query(
            "UPDATE channel_videos SET is_deleted = 1 \
              WHERE channel_id = 'UC1' AND video_id = 'vB'",
        )
        .execute(&pool)
        .await
        .unwrap();

        let feed = feed_for_child(&pool, child, 10).await.unwrap();
        let ids: Vec<&str> = feed.iter().map(|i| i.video_id.as_str()).collect();
        assert_eq!(ids, vec!["vA"]);
    }

    #[tokio::test]
    async fn feed_for_child_orders_null_published_at_by_last_seen_at() {
        let pool = setup_db().await;
        let child = insert_child(&pool, "kid").await;

        upsert_channel(&pool, "UC1").await.unwrap();
        upsert_channel(&pool, "UC2").await.unwrap();

        // UC1: RSS item from long ago.
        upsert_channel_videos_from_rss(&pool, "UC1", &[mk_item("vOld", 100)], 100)
            .await
            .unwrap();
        // UC2: sidecar-style item with NULL published_at, fetched
        // much more recently.
        let sidecar = ItemRow {
            video_id: "vNew".into(),
            title: "title-vNew".into(),
            channel_id: Some("UC2".into()),
            channel_title: Some("Channel Two".into()),
            thumbnail_url: Some("https://t/x.jpg".into()),
            published_at: None,
            published_raw: Some("3 days ago".into()),
        };
        upsert_channel_videos_from_sidecar(&pool, "UC2", &[sidecar], 10_000)
            .await
            .unwrap();

        allow_channel(&pool, child, "UC1").await;
        allow_channel(&pool, child, "UC2").await;

        let feed = feed_for_child(&pool, child, 10).await.unwrap();
        let ids: Vec<&str> = feed.iter().map(|i| i.video_id.as_str()).collect();
        assert_eq!(ids, vec!["vNew", "vOld"]);
    }

    #[tokio::test]
    async fn feed_for_child_excludes_hidden_videos() {
        let pool = setup_db().await;
        let child = insert_child(&pool, "kid").await;
        upsert_channel(&pool, "UC1").await.unwrap();
        upsert_channel_videos_from_rss(&pool, "UC1", &[mk_item("vA", 100), mk_item("vB", 200)], 0)
            .await
            .unwrap();
        allow_channel(&pool, child, "UC1").await;

        sqlx::query(
            "INSERT INTO hidden_videos (child_account_id, video_id) \
             VALUES (?, 'vB')",
        )
        .bind(child)
        .execute(&pool)
        .await
        .unwrap();

        let feed = feed_for_child(&pool, child, 10).await.unwrap();
        let ids: Vec<&str> = feed.iter().map(|i| i.video_id.as_str()).collect();
        assert_eq!(ids, vec!["vA"]);
    }

    #[tokio::test]
    async fn claim_due_sources_leases_so_concurrent_claim_skips() {
        let pool = setup_db().await;
        for id in ["UC1", "UC2", "UC3"] {
            upsert_channel(&pool, id).await.unwrap();
        }
        let now = 1_000;
        let first = claim_due_sources(&pool, now, 10, 60).await.unwrap();
        assert_eq!(first.len(), 3);
        let second = claim_due_sources(&pool, now, 10, 60).await.unwrap();
        assert!(
            second.is_empty(),
            "leased rows must not be re-claimed within the lease window"
        );

        let later = now + 120;
        let third = claim_due_sources(&pool, later, 10, 60).await.unwrap();
        assert_eq!(third.len(), 3);
    }

    #[tokio::test]
    async fn record_success_resets_errors_and_updates_etag() {
        let pool = setup_db().await;
        upsert_channel(&pool, "UC1").await.unwrap();
        record_poll_failure(&pool, "UC1", "boom", 100, 50)
            .await
            .unwrap();
        record_poll_failure(&pool, "UC1", "boom", 200, 60)
            .await
            .unwrap();
        let errs: i64 = sqlx::query_scalar(
            "SELECT rss_consecutive_errors FROM channels WHERE channel_id='UC1'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(errs, 2);

        record_poll_success(
            &pool,
            PollSuccess {
                channel_id: "UC1",
                title: Some("Channel Title"),
                etag: Some("etag-xyz"),
                last_modified: Some("Mon, 01 Jan 2024 00:00:00 GMT"),
                next_poll_at: 999,
                now: 70,
            },
        )
        .await
        .unwrap();

        let (errs, etag, title): (i64, Option<String>, Option<String>) = sqlx::query_as(
            "SELECT rss_consecutive_errors, rss_etag, channel_title \
               FROM channels WHERE channel_id='UC1'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(errs, 0);
        assert_eq!(etag.as_deref(), Some("etag-xyz"));
        assert_eq!(title.as_deref(), Some("Channel Title"));
    }

    #[tokio::test]
    async fn record_poll_deferred_preserves_consecutive_errors() {
        let pool = setup_db().await;
        upsert_channel(&pool, "UCkeep").await.unwrap();
        sqlx::query(
            "UPDATE channels SET rss_consecutive_errors = 2 \
              WHERE channel_id = 'UCkeep'",
        )
        .execute(&pool)
        .await
        .unwrap();

        record_poll_deferred(&pool, "UCkeep", "rate-capped", 999, 50)
            .await
            .unwrap();

        let (errs, last_polled, last_err, next): (i64, Option<i64>, Option<String>, i64) =
            sqlx::query_as(
                "SELECT rss_consecutive_errors, rss_last_polled_at, rss_last_error, rss_next_poll_at \
                   FROM channels WHERE channel_id='UCkeep'",
            )
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(errs, 2);
        assert_eq!(last_polled, Some(50));
        assert_eq!(last_err.as_deref(), Some("rate-capped"));
        assert_eq!(next, 999);
    }

    #[tokio::test]
    async fn record_source_dead_pushes_next_poll_and_clears_errors() {
        let pool = setup_db().await;
        upsert_channel(&pool, "UCdead").await.unwrap();
        record_poll_failure(&pool, "UCdead", "404", 100, 50)
            .await
            .unwrap();
        record_poll_failure(&pool, "UCdead", "404", 200, 60)
            .await
            .unwrap();

        record_source_dead(&pool, "UCdead", "channel not found", 9_999_999, 70)
            .await
            .unwrap();

        let (errs, next, err): (i64, i64, Option<String>) = sqlx::query_as(
            "SELECT rss_consecutive_errors, rss_next_poll_at, rss_last_error \
               FROM channels WHERE channel_id='UCdead'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(errs, 0);
        assert_eq!(next, 9_999_999);
        assert_eq!(err.as_deref(), Some("channel not found"));
    }

    #[tokio::test]
    async fn record_sidecar_fallback_persists_timestamp() {
        let pool = setup_db().await;
        upsert_channel(&pool, "UCfb").await.unwrap();
        record_sidecar_fallback_dispatched(&pool, "UCfb", 12345)
            .await
            .unwrap();
        let ts: Option<i64> = sqlx::query_scalar(
            "SELECT last_sidecar_fallback_at FROM channels \
              WHERE channel_id='UCfb'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(ts, Some(12345));

        let claimed = claim_due_sources(&pool, 1_000_000, 10, 60).await.unwrap();
        let row = claimed
            .iter()
            .find(|s| s.channel_id == "UCfb")
            .expect("UCfb is due");
        assert_eq!(row.last_sidecar_fallback_at, Some(12345));
    }

    #[tokio::test]
    async fn capacity_counts_aggregates_in_one_query() {
        let pool = setup_db().await;
        for id in ["UCq1", "UCq2", "UCq3"] {
            upsert_channel(&pool, id).await.unwrap();
        }
        let now: i64 = 100_000;
        sqlx::query(
            "UPDATE channels SET rss_next_poll_at = ?, rss_last_polled_at = ? \
              WHERE channel_id = 'UCq1'",
        )
        .bind(now - 60)
        .bind(now - 3500)
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            "UPDATE channels SET rss_next_poll_at = ?, rss_last_polled_at = ? \
              WHERE channel_id = 'UCq2'",
        )
        .bind(now + 1800)
        .bind(now - 600)
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query("UPDATE channels SET rss_next_poll_at = ? WHERE channel_id = 'UCq3'")
            .bind(now + 3600)
            .execute(&pool)
            .await
            .unwrap();
        record_sidecar_fallback_dispatched(&pool, "UCq3", now - 600)
            .await
            .unwrap();

        let counts = capacity_counts(&pool, now).await.unwrap();
        assert_eq!(counts.total_sources, 3);
        assert_eq!(counts.queue_depth, 1);
        assert_eq!(counts.polls_last_hour, 2);
        assert_eq!(counts.sidecar_fallbacks_last_hour, 1);
    }

    #[tokio::test]
    async fn capacity_counts_handles_empty_table() {
        let pool = setup_db().await;
        let counts = capacity_counts(&pool, 0).await.unwrap();
        assert_eq!(counts.total_sources, 0);
        assert_eq!(counts.queue_depth, 0);
        assert_eq!(counts.polls_last_hour, 0);
        assert_eq!(counts.sidecar_fallbacks_last_hour, 0);
    }

    #[tokio::test]
    async fn sidecar_fallbacks_in_last_hour_counts_only_recent() {
        let pool = setup_db().await;
        for id in ["UCa", "UCb", "UCc"] {
            upsert_channel(&pool, id).await.unwrap();
        }
        record_sidecar_fallback_dispatched(&pool, "UCa", 9_500)
            .await
            .unwrap();
        record_sidecar_fallback_dispatched(&pool, "UCb", 6_500)
            .await
            .unwrap();
        record_sidecar_fallback_dispatched(&pool, "UCc", 1_000)
            .await
            .unwrap();

        let n = sidecar_fallbacks_in_last_hour(&pool, 10_000).await.unwrap();
        assert_eq!(n, 2);
    }
}
