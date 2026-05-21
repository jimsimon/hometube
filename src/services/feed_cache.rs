//! DB layer for the push-on-schedule new-videos feed.
//!
//! Two tables back this module (see `migrations/009_feed_source_cache.sql`):
//!
//! - `feed_sources`        — one row per (kind, source_id) currently
//!   allowlisted by any child. Carries poll metadata (ETag, last
//!   success, next scheduled poll).
//! - `feed_source_items`   — the most recent N videos per source.
//!
//! This module is pure DB; no networking, no scheduling decisions. The
//! [`crate::services::feed_refresher`] task drives polling and calls
//! [`replace_source_items`] / [`record_poll_success`] /
//! [`record_poll_failure`] here. The `/api/feed/new-videos` handler
//! reads via [`feed_for_child`].

use sqlx::SqlitePool;

use crate::error::AppResult;
use crate::routes::feed::NewVideoItem;

/// Items kept per source after [`replace_source_items`] trims.
pub const PER_SOURCE_CAP: i64 = 20;

/// `kind` values used in `feed_sources.kind` / `feed_source_items.kind`.
pub const KIND_CHANNEL: &str = "channel";
pub const KIND_PLAYLIST: &str = "playlist";

/// One item destined for `feed_source_items`. Built by the source-
/// specific fetchers (RSS, sidecar) and handed to
/// [`replace_source_items`] in a batch.
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

/// One row from `feed_sources`, used by the refresher when picking work.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct DueSource {
    pub kind: String,
    pub source_id: String,
    pub etag: Option<String>,
    pub last_modified: Option<String>,
    pub consecutive_errors: i64,
    /// Unix-seconds timestamp of the most recent sidecar fallback for
    /// this source (`NULL` if none has ever happened). Used by the
    /// refresher to enforce the per-source rate cap without a second
    /// query.
    pub last_sidecar_fallback_at: Option<i64>,
}

/// One row of `feed_sources` for the admin diagnostics endpoint.
#[derive(Debug, Clone, sqlx::FromRow, serde::Serialize)]
pub struct FeedSourceStatus {
    pub kind: String,
    pub source_id: String,
    pub title: Option<String>,
    pub last_polled_at: Option<i64>,
    pub last_success_at: Option<i64>,
    pub last_error: Option<String>,
    pub consecutive_errors: i64,
    pub next_poll_at: i64,
    pub item_count: i64,
    /// Unix-seconds timestamp of the most recent sidecar fallback for
    /// this source, or `NULL` if none has ever happened. Surfaced so
    /// the diagnostics UI can show "last fallback: 5m ago" and the
    /// operator can correlate sidecar load with RSS outage windows.
    pub last_sidecar_fallback_at: Option<i64>,
}

/// Insert a `(kind, source_id)` row if missing. Sets `next_poll_at = 0`
/// so the refresher picks it up on its next tick. Idempotent.
pub async fn upsert_source(pool: &SqlitePool, kind: &str, source_id: &str) -> AppResult<()> {
    sqlx::query(
        "INSERT INTO feed_sources (kind, source_id, next_poll_at) \
         VALUES (?, ?, 0) \
         ON CONFLICT(kind, source_id) DO NOTHING",
    )
    .bind(kind)
    .bind(source_id)
    .execute(pool)
    .await?;
    Ok(())
}

/// Delete any `feed_sources` rows whose `(kind, source_id)` no longer
/// appears in the relevant allowlist table. Items cascade via foreign
/// key. Returns the number of sources removed.
pub async fn gc_orphan_sources(pool: &SqlitePool) -> AppResult<u64> {
    let result = sqlx::query(
        "DELETE FROM feed_sources \
         WHERE (kind = 'channel' AND source_id NOT IN (SELECT channel_id FROM allowlisted_channels)) \
            OR (kind = 'playlist' AND source_id NOT IN (SELECT playlist_id FROM allowlisted_playlists))",
    )
    .execute(pool)
    .await?;
    Ok(result.rows_affected())
}

/// Replace this source's items with `items`, then trim down to the most
/// recent [`PER_SOURCE_CAP`]. Runs in a single transaction so the table
/// is never observed in a half-updated state.
///
/// "Replace" here means upsert by `video_id`; we don't blow away rows
/// the source no longer mentions, because some YouTube views (e.g.
/// channel RSS) only ever return the 15 most recent uploads and we
/// want history to extend a little further than that.
pub async fn replace_source_items(
    pool: &SqlitePool,
    kind: &str,
    source_id: &str,
    items: &[ItemRow],
    now: i64,
) -> AppResult<()> {
    let mut tx = pool.begin().await?;

    for item in items {
        sqlx::query(
            "INSERT INTO feed_source_items \
                 (kind, source_id, video_id, title, channel_id, channel_title, \
                  thumbnail_url, published_at, published_raw, fetched_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?) \
             ON CONFLICT(kind, source_id, video_id) DO UPDATE SET \
                 title          = excluded.title, \
                 channel_id     = excluded.channel_id, \
                 channel_title  = excluded.channel_title, \
                 thumbnail_url  = excluded.thumbnail_url, \
                 published_at   = excluded.published_at, \
                 published_raw  = excluded.published_raw, \
                 fetched_at     = excluded.fetched_at",
        )
        .bind(kind)
        .bind(source_id)
        .bind(&item.video_id)
        .bind(&item.title)
        .bind(&item.channel_id)
        .bind(&item.channel_title)
        .bind(&item.thumbnail_url)
        .bind(item.published_at)
        .bind(&item.published_raw)
        .bind(now)
        .execute(&mut *tx)
        .await?;
    }

    // Trim down to PER_SOURCE_CAP most recent. When `published_at`
    // is NULL (sidecar fallback path, where YouTube only gave us a
    // relative time string like "3 days ago"), fall back to
    // `fetched_at` so sidecar-sourced rows aren't all treated as
    // epoch-old and discarded ahead of genuinely older RSS rows.
    sqlx::query(
        "DELETE FROM feed_source_items \
         WHERE kind = ? AND source_id = ? AND video_id NOT IN ( \
             SELECT video_id FROM feed_source_items \
              WHERE kind = ? AND source_id = ? \
              ORDER BY COALESCE(published_at, fetched_at) DESC, fetched_at DESC \
              LIMIT ? \
         )",
    )
    .bind(kind)
    .bind(source_id)
    .bind(kind)
    .bind(source_id)
    .bind(PER_SOURCE_CAP)
    .execute(&mut *tx)
    .await?;

    tx.commit().await?;
    Ok(())
}

/// Inputs to [`record_poll_success`]. Bundled into a struct so the
/// (otherwise 8-positional-arg) call site stays readable.
#[derive(Debug)]
pub struct PollSuccess<'a> {
    pub kind: &'a str,
    pub source_id: &'a str,
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
        kind,
        source_id,
        title,
        etag,
        last_modified,
        next_poll_at,
        now,
    } = args;
    sqlx::query(
        "UPDATE feed_sources SET \
             title              = COALESCE(?, title), \
             etag               = ?, \
             last_modified      = ?, \
             last_polled_at     = ?, \
             last_success_at    = ?, \
             last_error         = NULL, \
             consecutive_errors = 0, \
             next_poll_at       = ? \
         WHERE kind = ? AND source_id = ?",
    )
    .bind(title)
    .bind(etag)
    .bind(last_modified)
    .bind(now)
    .bind(now)
    .bind(next_poll_at)
    .bind(kind)
    .bind(source_id)
    .execute(pool)
    .await?;
    Ok(())
}

/// Mark a poll as **deferred-by-policy** — no network call attempted,
/// row rescheduled, **and** `consecutive_errors` left untouched. Used
/// when the refresher's rate caps temporarily deny a fallback for an
/// otherwise-eligible source: the source isn't in error and shouldn't
/// reset any existing error count from prior real failures, it just
/// has to wait its turn.
///
/// Distinct from [`record_poll_skipped`], which *does* clear errors —
/// that's the right semantics for the "we have no transport for this
/// kind" path, where the row genuinely isn't failing, but the wrong
/// semantics for "we briefly throttled this source."
pub async fn record_poll_deferred(
    pool: &SqlitePool,
    kind: &str,
    source_id: &str,
    reason: &str,
    next_poll_at: i64,
    now: i64,
) -> AppResult<()> {
    sqlx::query(
        "UPDATE feed_sources SET \
             last_polled_at = ?, \
             last_error     = ?, \
             next_poll_at   = ? \
         WHERE kind = ? AND source_id = ?",
    )
    .bind(now)
    .bind(reason)
    .bind(next_poll_at)
    .bind(kind)
    .bind(source_id)
    .execute(pool)
    .await?;
    Ok(())
}

/// Mark a poll as **skipped** — no network I/O happened, but the row
/// is rescheduled into the future and tagged so the diagnostics UI
/// can distinguish skipped-by-policy from a recent successful poll.
///
/// Used for sources whose `kind` the refresher does not yet support
/// (currently anything except `channel`). Crucially, this does NOT
/// touch `last_polled_at` / `last_success_at` so the diagnostics page
/// doesn't pretend a network round-trip occurred.
pub async fn record_poll_skipped(
    pool: &SqlitePool,
    kind: &str,
    source_id: &str,
    reason: &str,
    next_poll_at: i64,
) -> AppResult<()> {
    // Also reset `consecutive_errors` so rows that previously accumulated
    // errors under the pre-fix deferred-playlist behaviour stop showing a
    // misleading non-zero error count on the diagnostics page once they
    // transition to the intentionally-skipped path.
    sqlx::query(
        "UPDATE feed_sources SET \
             last_error         = ?, \
             consecutive_errors = 0, \
             next_poll_at       = ? \
         WHERE kind = ? AND source_id = ?",
    )
    .bind(reason)
    .bind(next_poll_at)
    .bind(kind)
    .bind(source_id)
    .execute(pool)
    .await?;
    Ok(())
}

/// Mark a poll as failed. Increments `consecutive_errors` and schedules
/// the next attempt. Does **not** clear cached items so the feed
/// continues to serve stale data through transient outages.
pub async fn record_poll_failure(
    pool: &SqlitePool,
    kind: &str,
    source_id: &str,
    err: &str,
    next_poll_at: i64,
    now: i64,
) -> AppResult<()> {
    sqlx::query(
        "UPDATE feed_sources SET \
             last_polled_at     = ?, \
             last_error         = ?, \
             consecutive_errors = consecutive_errors + 1, \
             next_poll_at       = ? \
         WHERE kind = ? AND source_id = ?",
    )
    .bind(now)
    .bind(err)
    .bind(next_poll_at)
    .bind(kind)
    .bind(source_id)
    .execute(pool)
    .await?;
    Ok(())
}

/// Mark the source as confidently dead: the sidecar returned a clean
/// "channel not found" / 404. Pushes `next_poll_at` far into the
/// future so the scheduler effectively shelves the row, clears the
/// error counter (it's not "failing" any more; it's *done*), and sets
/// `last_error` to a human-readable reason for the diagnostics UI.
///
/// We deliberately do not delete the row or its items: a future
/// reactivation (channel restored, or operator manually reschedules)
/// can pick it back up. The `feed_for_child` query will simply stop
/// surfacing items once the upstream stops producing them.
pub async fn record_source_dead(
    pool: &SqlitePool,
    kind: &str,
    source_id: &str,
    reason: &str,
    next_poll_at: i64,
    now: i64,
) -> AppResult<()> {
    sqlx::query(
        "UPDATE feed_sources SET \
             last_polled_at     = ?, \
             last_error         = ?, \
             consecutive_errors = 0, \
             next_poll_at       = ? \
         WHERE kind = ? AND source_id = ?",
    )
    .bind(now)
    .bind(reason)
    .bind(next_poll_at)
    .bind(kind)
    .bind(source_id)
    .execute(pool)
    .await?;
    Ok(())
}

/// Record that a sidecar fallback was dispatched for this source.
/// Called *before* the sidecar request goes out so concurrent claims
/// (in the rare burst case) see the reservation and respect the
/// per-source cap. The timestamp is also persisted across process
/// restarts so a `docker restart` or `cargo watch` rebuild can't
/// re-enable fallback for every source.
pub async fn record_sidecar_fallback_dispatched(
    pool: &SqlitePool,
    kind: &str,
    source_id: &str,
    now: i64,
) -> AppResult<()> {
    sqlx::query(
        "UPDATE feed_sources SET last_sidecar_fallback_at = ? \
         WHERE kind = ? AND source_id = ?",
    )
    .bind(now)
    .bind(kind)
    .bind(source_id)
    .execute(pool)
    .await?;
    Ok(())
}

/// Count sidecar fallbacks dispatched in the last hour, used by the
/// refresher to enforce the aggregate per-hour cap. Reads the same
/// `last_sidecar_fallback_at` column the per-source cap consults, so
/// "fallback fires" and "fallback would have been allowed" cannot
/// drift apart across restarts.
///
/// We store only the most recent timestamp per source (not a history),
/// so this query counts *sources that fell back in the last hour*
/// rather than *individual calls*. That's accurate as long as the
/// per-source rate cap is at least one hour: under that assumption a
/// single source can contribute at most one fallback to the count.
/// If the per-source interval is lowered below 3600 s (allowed by
/// `RANGE_SIDECAR_FALLBACK_MIN_INTERVAL_S` down to 60 s) the aggregate
/// cap will undercount, which is the safe direction — extra calls
/// would be permitted only by the per-source cap, never blocked by a
/// stricter-than-intended aggregate cap.
pub async fn sidecar_fallbacks_in_last_hour(pool: &SqlitePool, now: i64) -> AppResult<i64> {
    let cutoff = now - 3600;
    let n: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM feed_sources \
         WHERE last_sidecar_fallback_at IS NOT NULL \
           AND last_sidecar_fallback_at >= ?",
    )
    .bind(cutoff)
    .fetch_one(pool)
    .await?;
    Ok(n)
}

/// Atomically claim up to `limit` sources whose `next_poll_at <= now`,
/// pushing their `next_poll_at` forward by `lease_secs` so a concurrent
/// caller (or the next iteration of the refresher loop) does not pick
/// them up while the poll is in flight.
///
/// The caller is expected to call [`record_poll_success`] or
/// [`record_poll_failure`] before the lease expires; both overwrite
/// `next_poll_at` with the real scheduled time.
///
/// SQLite's `UPDATE ... RETURNING` (available since 3.35) gives us
/// the affected rows in a single statement, avoiding a TOCTOU between
/// SELECT and UPDATE.
pub async fn claim_due_sources(
    pool: &SqlitePool,
    now: i64,
    limit: i64,
    lease_secs: i64,
) -> AppResult<Vec<DueSource>> {
    let leased_until = now.saturating_add(lease_secs);
    let rows = sqlx::query_as::<_, DueSource>(
        "UPDATE feed_sources SET next_poll_at = ? \
          WHERE (kind, source_id) IN ( \
              SELECT kind, source_id FROM feed_sources \
               WHERE next_poll_at <= ? \
               ORDER BY next_poll_at ASC \
               LIMIT ? \
          ) \
          RETURNING kind, source_id, etag, last_modified, \
                    consecutive_errors, last_sidecar_fallback_at",
    )
    .bind(leased_until)
    .bind(now)
    .bind(limit)
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

/// Build the new-videos feed for a child. Joins `feed_source_items`
/// against the child's allowlist tables, excludes blocked + hidden
/// videos, dedupes by `video_id` keeping the most recent
/// `published_at`, and sorts/limits.
///
/// Dedupe is performed inside SQL via a `ROW_NUMBER()` window
/// function so the outer `LIMIT` applies to already-deduped rows and
/// cannot be eaten by duplicates that survive into Rust.
pub async fn feed_for_child(
    pool: &SqlitePool,
    child_id: i64,
    limit: usize,
) -> AppResult<Vec<NewVideoItem>> {
    #[derive(sqlx::FromRow)]
    struct Row {
        video_id: String,
        title: String,
        channel_id: Option<String>,
        channel_title: Option<String>,
        thumbnail_url: Option<String>,
        published_raw: Option<String>,
        #[allow(dead_code)]
        published_at: Option<i64>,
        kind: String,
        source_id: String,
    }

    // Window-function dedupe collapses duplicates inside SQL, so the
    // LIMIT applies to already-deduped rows.
    let fetch_limit = limit as i64;

    // Excludes both parent-controlled `blocked_videos` and per-child
    // `hidden_videos` so the row matches the visibility rules the old
    // `can_child_view`-based handler enforced. Dedupes by `video_id`
    // inside the query (a video appearing in multiple allowed sources
    // is collapsed to its newest copy) so the `LIMIT` cannot be eaten
    // by duplicates and produce a short result.
    let rows: Vec<Row> = sqlx::query_as(
        "WITH allowed(kind, source_id) AS ( \
             SELECT 'channel', channel_id \
               FROM allowlisted_channels WHERE child_account_id = ?1 \
             UNION ALL \
             SELECT 'playlist', playlist_id \
               FROM allowlisted_playlists WHERE child_account_id = ?1 \
         ), \
         candidates AS ( \
             SELECT i.video_id, i.title, i.channel_id, i.channel_title, \
                    i.thumbnail_url, i.published_raw, i.published_at, \
                    i.fetched_at, i.kind, i.source_id, \
                    ROW_NUMBER() OVER ( \
                        PARTITION BY i.video_id \
                        ORDER BY COALESCE(i.published_at, i.fetched_at) DESC, i.fetched_at DESC \
                    ) AS rn \
               FROM feed_source_items i \
               JOIN allowed a ON a.kind = i.kind AND a.source_id = i.source_id \
              WHERE NOT EXISTS ( \
                    SELECT 1 FROM blocked_videos b \
                     WHERE b.child_account_id = ?1 AND b.video_id = i.video_id) \
                AND NOT EXISTS ( \
                    SELECT 1 FROM hidden_videos h \
                     WHERE h.child_account_id = ?1 AND h.video_id = i.video_id) \
         ) \
         SELECT video_id, title, channel_id, channel_title, \
                thumbnail_url, published_raw, published_at, kind, source_id \
           FROM candidates WHERE rn = 1 \
          ORDER BY COALESCE(published_at, fetched_at) DESC, fetched_at DESC \
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
            channel_id: r.channel_id,
            channel_title: r.channel_title,
            thumbnail_url: r.thumbnail_url,
            published_at: r.published_raw,
            source_kind: r.kind,
            source_id: r.source_id,
        });
    }
    Ok(out)
}

/// Capacity-utilisation snapshot, used by the diagnostics endpoint
/// (and the parent settings UI) to surface "are we keeping up?" signal
/// in a single round-trip. All counts are derived from `feed_sources`
/// and aren't cached anywhere — the table is small enough (a few
/// thousand rows at most) that an aggregate count is sub-millisecond.
#[derive(Debug, Clone, sqlx::FromRow, serde::Serialize)]
pub struct FeedRefresherCapacityCounts {
    /// Total rows in `feed_sources`.
    pub total_sources: i64,
    /// Sources whose `next_poll_at <= now` *right now* — anything
    /// non-zero means the dispatcher hasn't drained the work yet.
    /// Persistent non-zero is the signal to lower `dispatch_delay_ms`
    /// or `channel_interval_s`.
    pub queue_depth: i64,
    /// Sources whose `last_polled_at` falls inside the last hour.
    /// Indicator of the dispatcher's actual throughput.
    pub polls_last_hour: i64,
    /// Sidecar fallbacks dispatched in the last hour. Mirrors what
    /// the aggregate-cap eligibility check sees, so the operator can
    /// correlate the diagnostics UI with the cap value.
    pub sidecar_fallbacks_last_hour: i64,
}

/// Compute the per-table capacity counts. All four counts come from
/// a single query plan against `feed_sources` so we don't pay an
/// extra round-trip per metric. The query uses conditional aggregates
/// rather than four separate `SELECT COUNT(*) ... WHERE ...` queries.
pub async fn capacity_counts(
    pool: &SqlitePool,
    now: i64,
) -> AppResult<FeedRefresherCapacityCounts> {
    let hour_ago = now - 3600;
    // COALESCE wraps the conditional sums so an empty `feed_sources`
    // table returns zeroes instead of NULLs (which would fail
    // `FromRow` on `i64` columns).
    let row: FeedRefresherCapacityCounts = sqlx::query_as(
        "SELECT \
             COUNT(*) AS total_sources, \
             COALESCE(SUM(CASE WHEN next_poll_at <= ? THEN 1 ELSE 0 END), 0) \
                 AS queue_depth, \
             COALESCE(SUM(CASE WHEN last_polled_at >= ? THEN 1 ELSE 0 END), 0) \
                 AS polls_last_hour, \
             COALESCE(SUM(CASE WHEN last_sidecar_fallback_at >= ? THEN 1 ELSE 0 END), 0) \
                 AS sidecar_fallbacks_last_hour \
           FROM feed_sources",
    )
    .bind(now)
    .bind(hour_ago)
    .bind(hour_ago)
    .fetch_one(pool)
    .await?;
    Ok(row)
}

/// Diagnostic snapshot of every cached source. Used by the admin
/// endpoint to surface poll health.
///
/// Uses a single LEFT JOIN against an aggregated subquery rather than
/// a per-row correlated subquery so the cost is O(N + M) instead of
/// O(N × index seeks).
pub async fn list_source_status(pool: &SqlitePool) -> AppResult<Vec<FeedSourceStatus>> {
    let rows = sqlx::query_as::<_, FeedSourceStatus>(
        "SELECT s.kind, s.source_id, s.title, s.last_polled_at, \
                s.last_success_at, s.last_error, s.consecutive_errors, \
                s.next_poll_at, \
                COALESCE(c.item_count, 0) AS item_count, \
                s.last_sidecar_fallback_at \
           FROM feed_sources s \
           LEFT JOIN ( \
                SELECT kind, source_id, COUNT(*) AS item_count \
                  FROM feed_source_items \
                 GROUP BY kind, source_id \
           ) c ON c.kind = s.kind AND c.source_id = s.source_id \
          ORDER BY s.kind ASC, s.source_id ASC",
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
        sqlx::query(
            "INSERT INTO allowlisted_channels \
                (child_account_id, channel_id, channel_title, added_by) \
             VALUES (?, ?, 'X', ?)",
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
        upsert_source(&pool, KIND_CHANNEL, "UC1").await.unwrap();
        upsert_source(&pool, KIND_CHANNEL, "UC1").await.unwrap();
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM feed_sources")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(count, 1);

        // No allowlist row → gc removes it.
        let removed = gc_orphan_sources(&pool).await.unwrap();
        assert_eq!(removed, 1);
    }

    #[tokio::test]
    async fn replace_items_trims_to_cap() {
        let pool = setup_db().await;
        upsert_source(&pool, KIND_CHANNEL, "UC1").await.unwrap();

        let items: Vec<ItemRow> = (0..(PER_SOURCE_CAP + 5))
            .map(|i| mk_item(&format!("v{i}"), 1000 + i))
            .collect();
        replace_source_items(&pool, KIND_CHANNEL, "UC1", &items, 9999)
            .await
            .unwrap();

        let n: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM feed_source_items WHERE kind='channel' AND source_id='UC1'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(n, PER_SOURCE_CAP);

        // Oldest (v0..v4) should be gone; newest kept.
        let kept: Vec<String> = sqlx::query_scalar(
            "SELECT video_id FROM feed_source_items WHERE kind='channel' AND source_id='UC1' \
             ORDER BY published_at DESC LIMIT 1",
        )
        .fetch_all(&pool)
        .await
        .unwrap();
        assert_eq!(kept[0], format!("v{}", PER_SOURCE_CAP + 4));
    }

    #[tokio::test]
    async fn feed_for_child_respects_allowlist_and_blocks() {
        let pool = setup_db().await;
        let child = insert_child(&pool, "kid").await;

        upsert_source(&pool, KIND_CHANNEL, "UC1").await.unwrap();
        upsert_source(&pool, KIND_CHANNEL, "UC2").await.unwrap();
        replace_source_items(
            &pool,
            KIND_CHANNEL,
            "UC1",
            &[mk_item("vA", 100), mk_item("vB", 200)],
            0,
        )
        .await
        .unwrap();
        replace_source_items(&pool, KIND_CHANNEL, "UC2", &[mk_item("vC", 300)], 0)
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
    async fn feed_for_child_orders_null_published_at_by_fetched_at() {
        // Regression: sidecar fallback writes items with
        // `published_at = NULL`. They must still surface near the top
        // when their `fetched_at` is recent, instead of being sorted
        // behind every RSS-timestamped item via COALESCE(..., 0).
        let pool = setup_db().await;
        let child = insert_child(&pool, "kid").await;

        upsert_source(&pool, KIND_CHANNEL, "UC1").await.unwrap();
        upsert_source(&pool, KIND_CHANNEL, "UC2").await.unwrap();

        // UC1: an RSS-timestamped item from "long ago".
        replace_source_items(&pool, KIND_CHANNEL, "UC1", &[mk_item("vOld", 100)], 100)
            .await
            .unwrap();
        // UC2: a sidecar-style item with NULL published_at, fetched
        // much more recently than vOld was published.
        let sidecar = ItemRow {
            video_id: "vNew".into(),
            title: "title-vNew".into(),
            channel_id: Some("UC2".into()),
            channel_title: Some("Channel Two".into()),
            thumbnail_url: Some("https://t/x.jpg".into()),
            published_at: None,
            published_raw: Some("3 days ago".into()),
        };
        replace_source_items(&pool, KIND_CHANNEL, "UC2", &[sidecar], 10_000)
            .await
            .unwrap();

        allow_channel(&pool, child, "UC1").await;
        allow_channel(&pool, child, "UC2").await;

        let feed = feed_for_child(&pool, child, 10).await.unwrap();
        let ids: Vec<&str> = feed.iter().map(|i| i.video_id.as_str()).collect();
        assert_eq!(
            ids,
            vec!["vNew", "vOld"],
            "sidecar item (NULL published_at, fetched_at=10000) should rank above RSS item (published_at=100)"
        );
    }

    #[tokio::test]
    async fn feed_for_child_excludes_hidden_videos() {
        let pool = setup_db().await;
        let child = insert_child(&pool, "kid").await;
        upsert_source(&pool, KIND_CHANNEL, "UC1").await.unwrap();
        replace_source_items(
            &pool,
            KIND_CHANNEL,
            "UC1",
            &[mk_item("vA", 100), mk_item("vB", 200)],
            0,
        )
        .await
        .unwrap();
        allow_channel(&pool, child, "UC1").await;

        // Hide vB; only vA should remain.
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
    async fn feed_dedupes_video_appearing_in_two_sources() {
        let pool = setup_db().await;
        let child = insert_child(&pool, "kid").await;

        upsert_source(&pool, KIND_CHANNEL, "UC1").await.unwrap();
        upsert_source(&pool, KIND_PLAYLIST, "PL1").await.unwrap();

        replace_source_items(&pool, KIND_CHANNEL, "UC1", &[mk_item("vSame", 100)], 0)
            .await
            .unwrap();
        replace_source_items(&pool, KIND_PLAYLIST, "PL1", &[mk_item("vSame", 200)], 0)
            .await
            .unwrap();

        allow_channel(&pool, child, "UC1").await;
        sqlx::query(
            "INSERT INTO allowlisted_playlists \
                (child_account_id, playlist_id, playlist_title, added_by) \
             VALUES (?, 'PL1', 'X', ?)",
        )
        .bind(child)
        .bind(child)
        .execute(&pool)
        .await
        .unwrap();

        let feed = feed_for_child(&pool, child, 10).await.unwrap();
        assert_eq!(feed.len(), 1, "expected dedupe");
        assert_eq!(feed[0].video_id, "vSame");
        // The newer (playlist) copy should win — its published_at=200,
        // which sorts ahead of the channel copy's published_at=100.
        assert!(feed[0]
            .published_at
            .as_deref()
            .map(|s| s.ends_with("200Z"))
            .unwrap_or(false));
    }

    #[tokio::test]
    async fn record_poll_skipped_does_not_touch_polled_or_success_columns() {
        let pool = setup_db().await;
        upsert_source(&pool, KIND_PLAYLIST, "PL1").await.unwrap();
        // Simulate legacy state: a non-zero consecutive_errors carried
        // over from the pre-fix code path that treated unsupported kinds
        // as failures. The skipped path must clear this so the
        // diagnostics page doesn't keep showing a misleading error
        // count.
        sqlx::query("UPDATE feed_sources SET consecutive_errors = 5 WHERE source_id = 'PL1'")
            .execute(&pool)
            .await
            .unwrap();
        record_poll_skipped(&pool, KIND_PLAYLIST, "PL1", "deferred", 12345)
            .await
            .unwrap();
        let (lp, ls, le, errs, next): (Option<i64>, Option<i64>, Option<String>, i64, i64) =
            sqlx::query_as(
                "SELECT last_polled_at, last_success_at, last_error, \
                    consecutive_errors, next_poll_at \
               FROM feed_sources WHERE kind='playlist' AND source_id='PL1'",
            )
            .fetch_one(&pool)
            .await
            .unwrap();
        assert!(
            lp.is_none(),
            "last_polled_at must not be set by skipped path"
        );
        assert!(
            ls.is_none(),
            "last_success_at must not be set by skipped path"
        );
        assert_eq!(le.as_deref(), Some("deferred"));
        assert_eq!(
            errs, 0,
            "skipped path must reset legacy consecutive_errors so diagnostics stop showing a stale non-zero count"
        );
        assert_eq!(next, 12345);
    }

    #[tokio::test]
    async fn claim_due_sources_leases_so_concurrent_claim_skips() {
        let pool = setup_db().await;
        for id in ["UC1", "UC2", "UC3"] {
            upsert_source(&pool, KIND_CHANNEL, id).await.unwrap();
        }
        // All three have next_poll_at = 0; the first claim takes all,
        // the second should return nothing because the lease pushed
        // them into the future.
        let now = 1_000;
        let first = claim_due_sources(&pool, now, 10, 60).await.unwrap();
        assert_eq!(first.len(), 3);
        let second = claim_due_sources(&pool, now, 10, 60).await.unwrap();
        assert!(
            second.is_empty(),
            "leased rows must not be re-claimed within the lease window"
        );

        // After the lease expires, they reappear.
        let later = now + 120;
        let third = claim_due_sources(&pool, later, 10, 60).await.unwrap();
        assert_eq!(third.len(), 3);
    }

    #[tokio::test]
    async fn record_success_resets_errors_and_updates_etag() {
        let pool = setup_db().await;
        upsert_source(&pool, KIND_CHANNEL, "UC1").await.unwrap();
        record_poll_failure(&pool, KIND_CHANNEL, "UC1", "boom", 100, 50)
            .await
            .unwrap();
        record_poll_failure(&pool, KIND_CHANNEL, "UC1", "boom", 200, 60)
            .await
            .unwrap();
        let errs: i64 = sqlx::query_scalar(
            "SELECT consecutive_errors FROM feed_sources WHERE kind='channel' AND source_id='UC1'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(errs, 2);

        record_poll_success(
            &pool,
            PollSuccess {
                kind: KIND_CHANNEL,
                source_id: "UC1",
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
            "SELECT consecutive_errors, etag, title \
               FROM feed_sources WHERE kind='channel' AND source_id='UC1'",
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
        // Rate-capped sources land here. The previous design used
        // `record_poll_skipped`, which clobbered `consecutive_errors`
        // — wrong for transient throttling because it would mask any
        // underlying failure history. `record_poll_deferred` must
        // leave the counter alone.
        let pool = setup_db().await;
        upsert_source(&pool, KIND_PLAYLIST, "PLkeep").await.unwrap();
        // Pre-seed two prior errors so we can prove they survive.
        sqlx::query(
            "UPDATE feed_sources SET consecutive_errors = 2 \
              WHERE kind = 'playlist' AND source_id = 'PLkeep'",
        )
        .execute(&pool)
        .await
        .unwrap();

        record_poll_deferred(&pool, KIND_PLAYLIST, "PLkeep", "rate-capped", 999, 50)
            .await
            .unwrap();

        let (errs, last_polled, last_err, next): (i64, Option<i64>, Option<String>, i64) =
            sqlx::query_as(
                "SELECT consecutive_errors, last_polled_at, last_error, next_poll_at \
                   FROM feed_sources WHERE kind='playlist' AND source_id='PLkeep'",
            )
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(
            errs, 2,
            "deferred path must preserve prior consecutive_errors"
        );
        assert_eq!(last_polled, Some(50));
        assert_eq!(last_err.as_deref(), Some("rate-capped"));
        assert_eq!(next, 999);
    }

    #[tokio::test]
    async fn record_source_dead_pushes_next_poll_and_clears_errors() {
        let pool = setup_db().await;
        upsert_source(&pool, KIND_CHANNEL, "UCdead").await.unwrap();
        // Accumulate some failures so we can prove they get cleared.
        record_poll_failure(&pool, KIND_CHANNEL, "UCdead", "404", 100, 50)
            .await
            .unwrap();
        record_poll_failure(&pool, KIND_CHANNEL, "UCdead", "404", 200, 60)
            .await
            .unwrap();

        record_source_dead(
            &pool,
            KIND_CHANNEL,
            "UCdead",
            "channel not found",
            9_999_999,
            70,
        )
        .await
        .unwrap();

        let (errs, next, err): (i64, i64, Option<String>) = sqlx::query_as(
            "SELECT consecutive_errors, next_poll_at, last_error \
               FROM feed_sources WHERE kind='channel' AND source_id='UCdead'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(errs, 0, "dead-channel path must clear the error counter");
        assert_eq!(next, 9_999_999);
        assert_eq!(err.as_deref(), Some("channel not found"));
    }

    #[tokio::test]
    async fn record_sidecar_fallback_persists_timestamp() {
        let pool = setup_db().await;
        upsert_source(&pool, KIND_CHANNEL, "UCfb").await.unwrap();
        record_sidecar_fallback_dispatched(&pool, KIND_CHANNEL, "UCfb", 12345)
            .await
            .unwrap();
        let ts: Option<i64> = sqlx::query_scalar(
            "SELECT last_sidecar_fallback_at FROM feed_sources \
              WHERE kind='channel' AND source_id='UCfb'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(ts, Some(12345));

        // And it round-trips through `claim_due_sources` so the
        // refresher can read it without an extra SELECT.
        let claimed = claim_due_sources(&pool, 1_000_000, 10, 60).await.unwrap();
        let row = claimed
            .iter()
            .find(|s| s.source_id == "UCfb")
            .expect("UCfb is due");
        assert_eq!(row.last_sidecar_fallback_at, Some(12345));
    }

    #[tokio::test]
    async fn capacity_counts_aggregates_in_one_query() {
        let pool = setup_db().await;
        // Three sources: one overdue, one polled recently, one fell
        // back recently. Lets us prove every CASE branch fires.
        for id in ["UCq1", "UCq2", "UCq3"] {
            upsert_source(&pool, KIND_CHANNEL, id).await.unwrap();
        }
        let now: i64 = 100_000;
        // UCq1 is overdue (next_poll_at < now) and was polled an hour ago.
        sqlx::query(
            "UPDATE feed_sources SET next_poll_at = ?, last_polled_at = ? \
              WHERE source_id = 'UCq1'",
        )
        .bind(now - 60)
        .bind(now - 3500)
        .execute(&pool)
        .await
        .unwrap();
        // UCq2 was polled 10 minutes ago, next poll in the future.
        sqlx::query(
            "UPDATE feed_sources SET next_poll_at = ?, last_polled_at = ? \
              WHERE source_id = 'UCq2'",
        )
        .bind(now + 1800)
        .bind(now - 600)
        .execute(&pool)
        .await
        .unwrap();
        // UCq3 fell back to the sidecar 10 minutes ago. Push its
        // next_poll_at forward so it doesn't count as overdue.
        sqlx::query("UPDATE feed_sources SET next_poll_at = ? WHERE source_id = 'UCq3'")
            .bind(now + 3600)
            .execute(&pool)
            .await
            .unwrap();
        record_sidecar_fallback_dispatched(&pool, KIND_CHANNEL, "UCq3", now - 600)
            .await
            .unwrap();

        let counts = capacity_counts(&pool, now).await.unwrap();
        assert_eq!(counts.total_sources, 3);
        assert_eq!(counts.queue_depth, 1, "only UCq1 is overdue");
        // UCq1 and UCq2 were both polled in the last hour. UCq3 has
        // no last_polled_at set so it doesn't count.
        assert_eq!(counts.polls_last_hour, 2);
        assert_eq!(counts.sidecar_fallbacks_last_hour, 1);
    }

    #[tokio::test]
    async fn capacity_counts_handles_empty_table() {
        // Empty feed_sources used to produce NULL from the conditional
        // SUMs; the COALESCE wrappers in the query keep the FromRow
        // derive happy.
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
            upsert_source(&pool, KIND_CHANNEL, id).await.unwrap();
        }
        // `now = 10_000`. Window starts at 10_000 - 3600 = 6_400.
        record_sidecar_fallback_dispatched(&pool, KIND_CHANNEL, "UCa", 9_500)
            .await
            .unwrap();
        record_sidecar_fallback_dispatched(&pool, KIND_CHANNEL, "UCb", 6_500)
            .await
            .unwrap();
        // UCc fell back well outside the window.
        record_sidecar_fallback_dispatched(&pool, KIND_CHANNEL, "UCc", 1_000)
            .await
            .unwrap();

        let n = sidecar_fallbacks_in_last_hour(&pool, 10_000).await.unwrap();
        assert_eq!(
            n, 2,
            "UCc fell back outside the 1h window and must not count"
        );
    }
}
