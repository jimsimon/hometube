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

    // Trim down to PER_SOURCE_CAP most recent. NULL `published_at`
    // sorts last via ORDER BY ... DESC NULLS LAST equivalent.
    sqlx::query(
        "DELETE FROM feed_source_items \
         WHERE kind = ? AND source_id = ? AND video_id NOT IN ( \
             SELECT video_id FROM feed_source_items \
              WHERE kind = ? AND source_id = ? \
              ORDER BY COALESCE(published_at, 0) DESC, fetched_at DESC \
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
          RETURNING kind, source_id, etag, last_modified, consecutive_errors",
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
/// We over-fetch with a small multiplier and dedupe in Rust rather
/// than depending on SQLite's window-function support, which keeps
/// this query simple and portable.
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

    // Over-fetch to account for dedupe collapse.
    let fetch_limit = (limit as i64).saturating_mul(3);

    // Excludes both parent-controlled `blocked_videos` and per-child
    // `hidden_videos` so the row matches the visibility rules the old
    // `can_child_view`-based handler enforced.
    let rows: Vec<Row> = sqlx::query_as(
        "WITH allowed(kind, source_id) AS ( \
             SELECT 'channel', channel_id \
               FROM allowlisted_channels WHERE child_account_id = ?1 \
             UNION ALL \
             SELECT 'playlist', playlist_id \
               FROM allowlisted_playlists WHERE child_account_id = ?1 \
         ) \
         SELECT i.video_id, i.title, i.channel_id, i.channel_title, \
                i.thumbnail_url, i.published_raw, i.published_at, \
                i.kind, i.source_id \
           FROM feed_source_items i \
           JOIN allowed a ON a.kind = i.kind AND a.source_id = i.source_id \
          WHERE NOT EXISTS ( \
                SELECT 1 FROM blocked_videos b \
                 WHERE b.child_account_id = ?1 AND b.video_id = i.video_id) \
            AND NOT EXISTS ( \
                SELECT 1 FROM hidden_videos h \
                 WHERE h.child_account_id = ?1 AND h.video_id = i.video_id) \
          ORDER BY COALESCE(i.published_at, 0) DESC \
          LIMIT ?2",
    )
    .bind(child_id)
    .bind(fetch_limit)
    .fetch_all(pool)
    .await?;

    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::with_capacity(limit);
    for r in rows {
        if !seen.insert(r.video_id.clone()) {
            continue;
        }
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
        if out.len() >= limit {
            break;
        }
    }
    Ok(out)
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
                COALESCE(c.item_count, 0) AS item_count \
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
        replace_source_items(
            &pool,
            KIND_CHANNEL,
            "UC2",
            &[mk_item("vC", 300)],
            0,
        )
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

        replace_source_items(
            &pool,
            KIND_CHANNEL,
            "UC1",
            &[mk_item("vSame", 100)],
            0,
        )
        .await
        .unwrap();
        replace_source_items(
            &pool,
            KIND_PLAYLIST,
            "PL1",
            &[mk_item("vSame", 200)],
            0,
        )
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
}
