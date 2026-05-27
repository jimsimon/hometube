//! Database connection + migration helpers.

use sqlx::{
    sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions},
    SqlitePool,
};
use std::str::FromStr;
use tracing::info;

/// Open a connection pool to the SQLite database, enabling WAL journaling
/// and foreign keys.
pub async fn connect(database_url: &str) -> anyhow::Result<SqlitePool> {
    let opts = SqliteConnectOptions::from_str(database_url)?
        .create_if_missing(true)
        .journal_mode(SqliteJournalMode::Wal)
        .foreign_keys(true);

    let pool = SqlitePoolOptions::new()
        .max_connections(8)
        .connect_with(opts)
        .await?;

    info!("connected to database");
    Ok(pool)
}

/// Run any pending migrations from the `migrations/` directory.
pub async fn migrate(pool: &SqlitePool) -> anyhow::Result<()> {
    sqlx::migrate!("./migrations").run(pool).await?;
    info!("migrations applied");
    audit_unresolved_video_stubs(pool).await;
    Ok(())
}

/// Count `videos` rows where the seed left `title == video_id` —
/// i.e., the row exists but no source (channel_videos, allowlist,
/// usage_log, watch_history, offline_downloads) ever carried a
/// non-blank title for it. Migration 024 fills these with the
/// `video_id` itself as a constraint-satisfying placeholder, but
/// these rows look like unresolved stubs to operators: they have no
/// real metadata, can't render in the UI past the bare id, and the
/// only way they get resolved is when a future writer (RSS poll,
/// heartbeat, allowlist add) carries an enriching title.
///
/// We log the count once at boot (post-migration) so operators have
/// visibility into how much of the seed landed in placeholder shape.
/// This is purely an observability hook — it does not mutate any
/// state and does not block boot if the query itself fails (we
/// `debug!`-log the failure since a missing `videos` table would
/// imply migrations didn't actually run, which the migrator above
/// would have already failed loudly on).
///
/// Cost: a single COUNT(*) with a WHERE filter — O(rows) without an
/// index, but only runs at boot and is bounded by table size which
/// is bounded by the user's lifetime watch history.
async fn audit_unresolved_video_stubs(pool: &SqlitePool) {
    let res: Result<i64, _> =
        sqlx::query_scalar("SELECT COUNT(*) FROM videos WHERE title = video_id")
            .fetch_one(pool)
            .await;
    match res {
        Ok(0) => {
            // Quiet on the happy path; an explicit zero would just
            // be noise in logs.
        }
        Ok(n) => {
            // Use `warn!` not `info!`: this isn't a routine status
            // line, it's "your DB has N rows that look like
            // unresolved stubs from the migration 024 seed; if this
            // number doesn't shrink over time, upstream metadata is
            // missing." Operators reading at `info!` would miss it.
            tracing::warn!(
                count = n,
                "videos rows with placeholder title (title == video_id) — \
                 migration 024 left these as unresolved stubs; they will be \
                 enriched by future writers (RSS, heartbeat, allowlist) as \
                 the data arrives. A persistently-non-zero count over time \
                 indicates upstream metadata gaps."
            );
        }
        Err(err) => {
            tracing::debug!(error = %err, "videos stub audit query failed");
        }
    }
}
