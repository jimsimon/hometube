//! Migration 020 data-preservation test.
//!
//! Bootstraps a fresh SQLite database, runs the migrator up to (but
//! not including) migration 020, seeds rows into the pre-020 tables
//! (`feed_sources` and `feed_source_items`), then runs migration 020
//! and asserts that the data has been faithfully copied into the new
//! `channel_sync_state` and `channel_videos` tables.
//!
//! sqlx's `Migrator::run` runs every migration in one shot, which
//! would drop the legacy tables before we get a chance to populate
//! them. Instead we drive the migrator manually, applying one
//! migration at a time so we can sandwich the seed step between
//! migration 019 (the last pre-channel-archive-sync migration) and
//! migration 020.

use sqlx::migrate::Migrator;
use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
use sqlx::{Executor, SqlitePool};
use std::path::Path;
use std::str::FromStr;

async fn open_pool() -> SqlitePool {
    let opts = SqliteConnectOptions::from_str("sqlite::memory:")
        .unwrap()
        .foreign_keys(true);
    SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(opts)
        .await
        .unwrap()
}

/// Apply migrations in order, but stop just before `stop_before_version`.
/// Re-uses sqlx's own `_sqlx_migrations` bookkeeping table so a
/// subsequent `Migrator::run` picks up where we left off, applying the
/// remaining migrations exactly the way production does.
async fn run_up_to(pool: &SqlitePool, migrator: &Migrator, stop_before_version: i64) {
    // Make sure the bookkeeping table exists.
    pool.execute(
        "CREATE TABLE IF NOT EXISTS _sqlx_migrations (\
             version BIGINT PRIMARY KEY, \
             description TEXT NOT NULL, \
             installed_on TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP, \
             success BOOLEAN NOT NULL, \
             checksum BLOB NOT NULL, \
             execution_time BIGINT NOT NULL \
         )",
    )
    .await
    .unwrap();

    for migration in migrator.iter() {
        if migration.version >= stop_before_version {
            break;
        }
        // Several migrations use the `COMMIT; PRAGMA …; BEGIN; …;
        // COMMIT; PRAGMA …; BEGIN;` pattern that relies on the
        // migration body being wrapped in an outer transaction by the
        // caller (sqlx does this for us in production). Replicate that
        // wrapping here so the first `COMMIT;` inside a migration body
        // has something to close.
        pool.execute("BEGIN").await.unwrap();
        pool.execute(&*migration.sql)
            .await
            .unwrap_or_else(|e| panic!("migration {} failed: {e}", migration.version));
        // Some migrations terminate with `BEGIN;` so sqlx's outer
        // commit finds an active transaction — close it now whether
        // the migration left one open or not.
        let _ = pool.execute("COMMIT").await; // ignore "no tx active" if migration already committed

        sqlx::query(
            "INSERT INTO _sqlx_migrations \
                 (version, description, success, checksum, execution_time) \
             VALUES (?, ?, 1, ?, 0)",
        )
        .bind(migration.version)
        .bind(migration.description.as_ref())
        .bind(migration.checksum.as_ref())
        .execute(pool)
        .await
        .unwrap();
    }
}

#[tokio::test]
async fn migration_020_preserves_feed_source_items_into_channel_videos() {
    let pool = open_pool().await;
    let migrator = Migrator::new(Path::new("./migrations")).await.unwrap();

    // 1. Apply migrations 001 .. 019 (pre-channel-archive-sync).
    run_up_to(&pool, &migrator, 20).await;

    // Sanity: pre-020 tables exist.
    let pre: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM sqlite_master \
          WHERE type = 'table' AND name IN ('feed_sources','feed_source_items')",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        pre, 2,
        "feed_sources + feed_source_items must exist pre-020"
    );

    // 2. Seed pre-020 data: two channels with assorted items, plus an
    //    allowlist row so the optional thumbnail_url backfill has data
    //    to copy from.
    sqlx::query(
        "INSERT INTO accounts (display_name, account_type, pin_hash, created_at, updated_at) \
         VALUES ('p', 'parent', 'x', unixepoch(), unixepoch())",
    )
    .execute(&pool)
    .await
    .unwrap();
    let parent_id: i64 = sqlx::query_scalar("SELECT last_insert_rowid()")
        .fetch_one(&pool)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO accounts (display_name, account_type, pin_hash, created_at, updated_at) \
         VALUES ('k', 'child', 'x', unixepoch(), unixepoch())",
    )
    .execute(&pool)
    .await
    .unwrap();
    let child_id: i64 = sqlx::query_scalar("SELECT last_insert_rowid()")
        .fetch_one(&pool)
        .await
        .unwrap();

    sqlx::query(
        "INSERT INTO allowlisted_channels \
            (child_account_id, channel_id, channel_title, channel_thumbnail_url, added_by) \
         VALUES (?, 'UC1', 'Channel One', 'https://t/uc1.jpg', ?), \
                (?, 'UC2', 'Channel Two', 'https://t/uc2.jpg', ?)",
    )
    .bind(child_id)
    .bind(parent_id)
    .bind(child_id)
    .bind(parent_id)
    .execute(&pool)
    .await
    .unwrap();

    // feed_sources rows with poll bookkeeping.
    sqlx::query(
        "INSERT INTO feed_sources \
             (kind, source_id, title, etag, last_modified, last_polled_at, \
              last_success_at, last_error, consecutive_errors, next_poll_at, \
              last_sidecar_fallback_at) \
         VALUES \
             ('channel', 'UC1', 'Channel One', '\"abc\"', 'Mon, 01 Jan 2024 00:00:00 GMT', \
              100, 100, NULL, 0, 200, NULL), \
             ('channel', 'UC2', 'Channel Two', NULL, NULL, 50, NULL, 'boom', 3, 300, 90)",
    )
    .execute(&pool)
    .await
    .unwrap();

    // feed_source_items rows. Some have channel_id set, some don't.
    sqlx::query(
        "INSERT INTO feed_source_items \
             (kind, source_id, video_id, title, channel_id, channel_title, \
              thumbnail_url, published_at, published_raw, fetched_at) \
         VALUES \
             ('channel', 'UC1', 'vA', 'A', 'UC1', 'Channel One', 'https://t/a.jpg', 1000, '2024-06-01', 1100), \
             ('channel', 'UC1', 'vB', 'B', NULL, 'Channel One', NULL, NULL, '3 days ago', 1200), \
             ('channel', 'UC2', 'vC', 'C', 'UC2', 'Channel Two', 'https://t/c.jpg', 2000, '2024-06-15', 2100)",
    )
    .execute(&pool)
    .await
    .unwrap();

    // 3. Apply the rest of the migrations (020 and onward).
    migrator.run(&pool).await.unwrap();

    // 4. Verify post-migration state.

    // 4a. The legacy tables are gone.
    let post: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM sqlite_master \
          WHERE type = 'table' AND name IN ('feed_sources','feed_source_items')",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        post, 0,
        "feed_sources + feed_source_items must be dropped post-020"
    );

    // 4b. channel_sync_state contains both channels with poll bookkeeping
    //     migrated and channel_thumbnail_url backfilled from
    //     allowlisted_channels.
    type SyncStateRow = (
        String,         // channel_id
        Option<String>, // channel_title
        Option<String>, // channel_thumbnail_url
        Option<String>, // rss_etag
        Option<i64>,    // rss_last_success_at
        i64,            // rss_consecutive_errors
        Option<String>, // rss_last_error
    );
    let states: Vec<SyncStateRow> = sqlx::query_as(
        "SELECT channel_id, channel_title, channel_thumbnail_url, rss_etag, \
                rss_last_success_at, rss_consecutive_errors, rss_last_error \
           FROM channel_sync_state ORDER BY channel_id",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(states.len(), 2);

    let (id1, title1, thumb1, etag1, lsa1, errs1, err1) = &states[0];
    assert_eq!(id1, "UC1");
    assert_eq!(title1.as_deref(), Some("Channel One"));
    assert_eq!(thumb1.as_deref(), Some("https://t/uc1.jpg"));
    assert_eq!(etag1.as_deref(), Some("\"abc\""));
    assert_eq!(*lsa1, Some(100));
    assert_eq!(*errs1, 0);
    assert!(err1.is_none());

    let (id2, _, thumb2, etag2, lsa2, errs2, err2) = &states[1];
    assert_eq!(id2, "UC2");
    assert_eq!(thumb2.as_deref(), Some("https://t/uc2.jpg"));
    assert!(etag2.is_none());
    assert!(lsa2.is_none());
    assert_eq!(*errs2, 3, "consecutive_errors must survive");
    assert_eq!(err2.as_deref(), Some("boom"));

    // 4c. channel_videos contains all three items. Rows with a NULL
    //     channel_id in feed_source_items fall back to the source_id.
    type VideoRow = (
        String, // channel_id
        String, // video_id
        String, // title
        String, // source
        i64,    // first_seen_at
        i64,    // last_seen_at
        String, // published_raw (via COALESCE)
        i64,    // is_deleted
    );
    let videos: Vec<VideoRow> = sqlx::query_as(
        "SELECT channel_id, video_id, title, source, first_seen_at, last_seen_at, \
                COALESCE(published_raw, ''), is_deleted \
           FROM channel_videos ORDER BY channel_id, video_id",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(videos.len(), 3);

    // vA: had channel_id='UC1' on the legacy row.
    assert_eq!(videos[0].0, "UC1");
    assert_eq!(videos[0].1, "vA");
    assert_eq!(videos[0].3, "rss", "migrated rows default to source='rss'");
    assert_eq!(videos[0].4, 1100, "first_seen_at = legacy fetched_at");
    assert_eq!(videos[0].5, 1100, "last_seen_at = legacy fetched_at");
    assert_eq!(videos[0].7, 0, "migrated rows are NOT tombstoned");

    // vB: had channel_id=NULL on the legacy row → COALESCE(channel_id, source_id)
    // pulls UC1 (the source_id) into channel_videos.channel_id.
    assert_eq!(videos[1].0, "UC1");
    assert_eq!(videos[1].1, "vB");
    assert_eq!(videos[1].6, "3 days ago");

    // vC: simple channel-2 row.
    assert_eq!(videos[2].0, "UC2");
    assert_eq!(videos[2].1, "vC");
    assert_eq!(videos[2].4, 2100);

    // 4d. Backfill bookkeeping starts at a known-pending state for
    //     every migrated channel so the new backfill loop picks them up
    //     on its next tick.
    let backfill: Vec<(String, String, i64)> = sqlx::query_as(
        "SELECT channel_id, backfill_status, backfill_next_at \
           FROM channel_sync_state ORDER BY channel_id",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    for (cid, status, next_at) in &backfill {
        assert_eq!(status, "pending", "{cid} must start pending");
        assert_eq!(*next_at, 0, "{cid} must be immediately due");
    }
}
