//! On-disk thumbnail cache.
//!
//! Modelled on [`crate::services::segment_store`]: small (`get`/`put`)
//! read+write primitives plus an LRU-driven cleanup invoked by the
//! existing `cache_cleanup` cron. The proxy route at
//! `src/routes/videos.rs::get_thumbnail` reads disk-first via [`get`]
//! and only fetches from YouTube on miss, calling [`put`] to populate.
//!
//! Video thumbnails live under `<cache_dir>/thumbnails/<video_id>.jpg`.
//! One blob per video — thumbnails are derived URLs
//! (`https://i.ytimg.com/vi/<id>/hqdefault.jpg`) so a single cache
//! entry covers every render of every video card.
//!
//! Channel avatars share the same `thumbnail_cache` table and LRU
//! machinery but live under `<cache_dir>/thumbnails/channels/<id>.jpg`
//! and use a `channel:`-prefixed primary key (see [`channel_cache_key`])
//! so a channel ID can never collide with a video ID in the shared
//! table. Their upstream URL is not derivable from the ID (it's a
//! `googleusercontent.com` / `ggpht.com` blob), so the proxy route
//! resolves it from `channels.channel_thumbnail_url` on a miss.

use std::path::PathBuf;

use sqlx::SqlitePool;
use tracing::{debug, warn};

use crate::error::AppResult;

/// Validate that `video_id` is a syntactically-plausible YouTube video
/// ID before interpolating it into a filesystem path. Cheap defence
/// against path traversal — even though the proxy route only reaches
/// `put` after `cache.get_or_extract` succeeds (yt-dlp's URL parser
/// would reject anything malformed), the backfill prefetch path
/// writes whatever video_id yt-dlp emitted in its `--flat-playlist`
/// JSON without re-validation. Defence in depth.
///
/// YouTube IDs are 11 chars from `[A-Za-z0-9_-]`. We accept slightly
/// looser (any length 1..=64 of the same character class) so a future
/// schema bump (longer IDs?) doesn't break the cache silently. The
/// critical property is "no `/`, no `..`, no `\0`" — anything that
/// could break out of `<cache_dir>/thumbnails/`.
fn is_safe_video_id(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 64
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

/// `app_config` key for the configured maximum total cache size in
/// bytes. `0` (or unset) means "no LRU eviction" — the cache grows
/// unbounded until cleared explicitly.
pub const KEY_THUMBNAIL_CACHE_MAX_BYTES: &str = "thumbnail_cache_max_bytes";

/// Default cache cap: 500 MiB. Small enough to fit on even a modest
/// home-server installation; large enough that a family with a few
/// hundred allowlisted channels can hold most of the channel-archive
/// thumbnails warm at once (~10–50 KB per thumbnail).
pub const DEFAULT_MAX_BYTES: i64 = 500 * 1024 * 1024;

/// Compute the on-disk path for a video's cached thumbnail. Lives
/// under `<cache_dir>/thumbnails/<video_id>.jpg`. The subdirectory is
/// created on first write by [`put`].
pub fn thumbnail_path(cache_dir: &str, video_id: &str) -> PathBuf {
    PathBuf::from(cache_dir)
        .join("thumbnails")
        .join(format!("{video_id}.jpg"))
}

/// Compute the on-disk path for a channel's cached avatar. Lives under
/// `<cache_dir>/thumbnails/channels/<channel_id>.jpg` — a separate
/// subdirectory from video thumbnails so the two keyspaces stay
/// visually distinct on disk. Created on first write by [`put_channel`].
pub fn channel_thumbnail_path(cache_dir: &str, channel_id: &str) -> PathBuf {
    PathBuf::from(cache_dir)
        .join("thumbnails")
        .join("channels")
        .join(format!("{channel_id}.jpg"))
}

/// Build the `thumbnail_cache` primary key for a channel avatar.
///
/// The table's `video_id` PK column is reused for channel entries; the
/// `channel:` prefix namespaces them so a 24-char channel ID can never
/// collide with an 11-char video ID, and so a future migration can
/// trivially partition the two. The prefix only ever appears as a DB
/// key — the on-disk filename is derived from the bare `channel_id` via
/// [`channel_thumbnail_path`].
fn channel_cache_key(channel_id: &str) -> String {
    format!("channel:{channel_id}")
}

/// Look up the cache entry for `video_id`. Returns the on-disk path
/// when a row exists, or `None` for a miss. The caller is expected to
/// read the bytes itself via [`tokio::fs::read`] and serve them to the
/// HTTP client — keeping the I/O out of this module makes the proxy
/// route streamable in the future without an API churn.
///
/// Also bumps `last_accessed_at` to `now` for LRU purposes. Best
/// effort — a touch failure does not fail the lookup.
///
/// Returns `None` if `video_id` doesn't look like a YouTube video ID
/// — the DB lookup is short-circuited so callers can rely on the
/// stored `file_path` being safe to read without re-validating the
/// id at every consumer.
pub async fn get(pool: &SqlitePool, video_id: &str) -> Option<PathBuf> {
    if !is_safe_video_id(video_id) {
        return None;
    }
    get_by_key(pool, video_id).await
}

/// Look up the cache entry for a channel avatar by `channel_id`.
/// Mirror of [`get`] for the channel keyspace — see
/// [`channel_cache_key`] for why the two can't collide. Returns the
/// on-disk path (under `thumbnails/channels/`) on a hit, or `None` on a
/// miss / unsafe id.
pub async fn get_channel(pool: &SqlitePool, channel_id: &str) -> Option<PathBuf> {
    if !is_safe_video_id(channel_id) {
        return None;
    }
    get_by_key(pool, &channel_cache_key(channel_id)).await
}

/// Shared lookup body for [`get`] / [`get_channel`]. `key` is the
/// `thumbnail_cache.video_id` primary key (a bare video ID, or a
/// `channel:`-prefixed channel key) — it is never used to build a
/// filesystem path, so the validation in the public wrappers is what
/// protects against path traversal.
async fn get_by_key(pool: &SqlitePool, key: &str) -> Option<PathBuf> {
    let row: Option<(String,)> =
        sqlx::query_as("SELECT file_path FROM thumbnail_cache WHERE video_id = ?")
            .bind(key)
            .fetch_optional(pool)
            .await
            .ok()
            .flatten();
    let (path,) = row?;
    // Verify the file still exists on disk — if a manual `rm` removed
    // it, prefer "cache miss" over "broken read". Cheap stat; only
    // happens on hits which are already the warm path.
    if tokio::fs::metadata(&path).await.is_err() {
        // Drop the orphan row so a future put() doesn't think the
        // entry is still valid.
        let _ = sqlx::query("DELETE FROM thumbnail_cache WHERE video_id = ?")
            .bind(key)
            .execute(pool)
            .await;
        return None;
    }
    // Bump LRU timestamp.
    let _ = sqlx::query(
        "UPDATE thumbnail_cache SET last_accessed_at = unixepoch() \
          WHERE video_id = ?",
    )
    .bind(key)
    .execute(pool)
    .await;
    Some(PathBuf::from(path))
}

/// Store `bytes` as the cached thumbnail for `video_id`. Writes to
/// `<cache_dir>/thumbnails/<video_id>.jpg` and records the row in
/// `thumbnail_cache`. Replaces any prior entry for the same video.
///
/// Best-effort: a filesystem error returns early without recording the
/// row, so the next request will retry from upstream rather than
/// serving a half-written file.
///
/// Rejects `video_id`s that don't match the YouTube ID character class
/// — defence against path traversal if a future caller forwards an
/// untrusted ID into the cache without intermediate validation.
pub async fn put(
    pool: &SqlitePool,
    cache_dir: &str,
    video_id: &str,
    bytes: &[u8],
) -> AppResult<()> {
    if !is_safe_video_id(video_id) {
        warn!(video_id, "thumbnail_store::put: refusing unsafe video_id");
        return Ok(());
    }
    put_by_key(pool, video_id, thumbnail_path(cache_dir, video_id), bytes).await
}

/// Store `bytes` as the cached avatar for `channel_id`. Mirror of
/// [`put`] for the channel keyspace — writes under
/// `<cache_dir>/thumbnails/channels/<channel_id>.jpg` and records a
/// `channel:`-prefixed row in `thumbnail_cache`.
pub async fn put_channel(
    pool: &SqlitePool,
    cache_dir: &str,
    channel_id: &str,
    bytes: &[u8],
) -> AppResult<()> {
    if !is_safe_video_id(channel_id) {
        warn!(
            channel_id,
            "thumbnail_store::put_channel: refusing unsafe channel_id"
        );
        return Ok(());
    }
    put_by_key(
        pool,
        &channel_cache_key(channel_id),
        channel_thumbnail_path(cache_dir, channel_id),
        bytes,
    )
    .await
}

/// Shared write body for [`put`] / [`put_channel`]. `key` is the
/// `thumbnail_cache.video_id` primary key; `path` is the already-built
/// (and validated, via the public wrappers) on-disk destination.
async fn put_by_key(pool: &SqlitePool, key: &str, path: PathBuf, bytes: &[u8]) -> AppResult<()> {
    if let Some(parent) = path.parent() {
        if let Err(e) = tokio::fs::create_dir_all(parent).await {
            warn!(error = %e, "thumbnail_store: failed to create thumbnails dir");
            return Ok(());
        }
    }

    // Write atomically: temp file + rename. Mirrors the segment_store
    // convention — avoids serving a partial file if the process is
    // killed mid-write.
    let tmp_path = path.with_extension("jpg.partial");
    if let Err(e) = tokio::fs::write(&tmp_path, bytes).await {
        warn!(error = %e, key, "thumbnail_store: write to tempfile failed");
        return Ok(());
    }
    if let Err(e) = tokio::fs::rename(&tmp_path, &path).await {
        warn!(error = %e, key, "thumbnail_store: rename into place failed");
        // Best-effort cleanup of the orphan tempfile.
        let _ = tokio::fs::remove_file(&tmp_path).await;
        return Ok(());
    }

    sqlx::query(
        "INSERT INTO thumbnail_cache (video_id, file_path, file_size_bytes, cached_at, last_accessed_at) \
         VALUES (?, ?, ?, unixepoch(), unixepoch()) \
         ON CONFLICT(video_id) DO UPDATE SET \
             file_path = excluded.file_path, \
             file_size_bytes = excluded.file_size_bytes, \
             last_accessed_at = excluded.last_accessed_at",
    )
    .bind(key)
    .bind(path.to_string_lossy().as_ref())
    .bind(bytes.len() as i64)
    .execute(pool)
    .await?;
    Ok(())
}

/// Diagnostic snapshot of the cache for the parent UI.
#[derive(Debug, Clone, sqlx::FromRow, serde::Serialize)]
pub struct ThumbnailCacheStats {
    pub entry_count: i64,
    pub total_bytes: i64,
}

pub async fn stats(pool: &SqlitePool) -> AppResult<ThumbnailCacheStats> {
    let row: ThumbnailCacheStats = sqlx::query_as(
        "SELECT COUNT(*) AS entry_count, \
                COALESCE(SUM(file_size_bytes), 0) AS total_bytes \
           FROM thumbnail_cache",
    )
    .fetch_one(pool)
    .await?;
    Ok(row)
}

/// LRU eviction down to `max_bytes`. Called by the existing
/// `cache_cleanup` cron handler. Returns `(evicted_count, bytes_freed)`.
///
/// `max_bytes <= 0` disables LRU (the cache grows unbounded, matching
/// the segment-cache "Unlimited" preset).
pub async fn cleanup_lru(pool: &SqlitePool, max_bytes: i64) -> AppResult<(u64, u64)> {
    if max_bytes <= 0 {
        return Ok((0, 0));
    }
    let mut current_total: i64 =
        sqlx::query_scalar("SELECT COALESCE(SUM(file_size_bytes), 0) FROM thumbnail_cache")
            .fetch_one(pool)
            .await
            .unwrap_or(0);
    if current_total <= max_bytes {
        return Ok((0, 0));
    }

    // Walk LRU order until under the cap.
    let rows: Vec<(String, String, i64)> = sqlx::query_as(
        "SELECT video_id, file_path, file_size_bytes \
           FROM thumbnail_cache ORDER BY last_accessed_at ASC",
    )
    .fetch_all(pool)
    .await?;

    let mut evicted_count: u64 = 0;
    let mut evicted_bytes: u64 = 0;
    for (video_id, file_path, size) in rows {
        if current_total <= max_bytes {
            break;
        }
        if let Err(e) = tokio::fs::remove_file(&file_path).await {
            debug!(%file_path, %e, "thumbnail_store: file already gone during eviction");
        }
        sqlx::query("DELETE FROM thumbnail_cache WHERE video_id = ?")
            .bind(&video_id)
            .execute(pool)
            .await?;
        current_total = current_total.saturating_sub(size);
        evicted_count += 1;
        // Saturating ops + clamp-to-zero guards against a negative
        // `file_size_bytes` (which the `CHECK (file_size_bytes >= 0)`
        // constraint on the column forbids, but belt-and-braces for
        // older rows written before the constraint was added or DBs
        // that bypassed the migration).
        let size_u64 = size.max(0) as u64;
        evicted_bytes = evicted_bytes.saturating_add(size_u64);
    }
    Ok((evicted_count, evicted_bytes))
}

/// Read the configured max-bytes from `app_config`. Falls back to the
/// default when unset or out of range.
pub async fn configured_max_bytes(pool: &SqlitePool) -> i64 {
    let raw: Option<String> = sqlx::query_scalar("SELECT value FROM app_config WHERE key = ?")
        .bind(KEY_THUMBNAIL_CACHE_MAX_BYTES)
        .fetch_optional(pool)
        .await
        .ok()
        .flatten();
    raw.as_deref()
        .and_then(|s| s.parse::<i64>().ok())
        .filter(|n| *n >= 0)
        .unwrap_or(DEFAULT_MAX_BYTES)
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
    use std::str::FromStr;
    use tempfile::TempDir;

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

    #[test]
    fn is_safe_video_id_rejects_traversal_and_separators() {
        // Accepted: real YouTube IDs and similar short alphanumeric+_- strings.
        assert!(is_safe_video_id("dQw4w9WgXcQ"));
        assert!(is_safe_video_id("abc_DEF-123"));
        assert!(is_safe_video_id("a"));

        // Rejected: anything that could escape `<cache_dir>/thumbnails/`.
        assert!(!is_safe_video_id(""));
        assert!(!is_safe_video_id(".."));
        assert!(!is_safe_video_id("../etc/passwd"));
        assert!(!is_safe_video_id("a/b"));
        assert!(!is_safe_video_id("a\\b"));
        assert!(!is_safe_video_id("a\0b"));
        assert!(!is_safe_video_id("a.b"));
        assert!(!is_safe_video_id("a b"));
        assert!(!is_safe_video_id(&"x".repeat(65))); // length cap
    }

    #[tokio::test]
    async fn put_refuses_unsafe_video_id() {
        let pool = setup_db().await;
        let cache = TempDir::new().unwrap();
        let dir = cache.path().to_str().unwrap();

        // Attempt to write to a path-traversing video_id.
        put(&pool, dir, "../escape", b"x").await.unwrap();

        // No row was inserted.
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM thumbnail_cache")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(count, 0);

        // And no file was written outside the cache dir.
        let escape_path = cache.path().parent().unwrap().join("escape.jpg");
        assert!(!escape_path.exists(), "must not write outside cache dir");
    }

    #[tokio::test]
    async fn get_refuses_unsafe_video_id() {
        let pool = setup_db().await;
        // Even if a row somehow exists with an unsafe id (e.g. raw SQL
        // injection or schema rollback), `get` doesn't return it.
        sqlx::query(
            "INSERT INTO thumbnail_cache \
                (video_id, file_path, file_size_bytes, cached_at, last_accessed_at) \
             VALUES ('../escape', '/etc/passwd', 1, 1, 1)",
        )
        .execute(&pool)
        .await
        .unwrap();
        assert!(get(&pool, "../escape").await.is_none());
    }

    #[tokio::test]
    async fn put_then_get_round_trips() {
        let pool = setup_db().await;
        let cache = TempDir::new().unwrap();
        let bytes = b"JFIF-thumbnail-bytes";

        put(&pool, cache.path().to_str().unwrap(), "vA", bytes)
            .await
            .unwrap();

        let path = get(&pool, "vA").await.expect("cache hit expected");
        let read_back = tokio::fs::read(&path).await.unwrap();
        assert_eq!(read_back, bytes);
    }

    #[tokio::test]
    async fn get_misses_when_no_row() {
        let pool = setup_db().await;
        assert!(get(&pool, "missing").await.is_none());
    }

    #[tokio::test]
    async fn channel_put_then_get_round_trips() {
        let pool = setup_db().await;
        let cache = TempDir::new().unwrap();
        let bytes = b"channel-avatar-bytes";

        put_channel(&pool, cache.path().to_str().unwrap(), "UCabcdef", bytes)
            .await
            .unwrap();

        let path = get_channel(&pool, "UCabcdef")
            .await
            .expect("channel cache hit expected");
        // Channel avatars live in their own subdirectory.
        assert!(path.to_string_lossy().contains("thumbnails/channels/"));
        let read_back = tokio::fs::read(&path).await.unwrap();
        assert_eq!(read_back, bytes);
    }

    #[tokio::test]
    async fn channel_and_video_keyspaces_do_not_collide() {
        let pool = setup_db().await;
        let cache = TempDir::new().unwrap();
        let dir = cache.path().to_str().unwrap();

        // Same bare id used for both a (hypothetical) video and a channel
        // must produce two independent cache entries.
        put(&pool, dir, "sharedId", b"video").await.unwrap();
        put_channel(&pool, dir, "sharedId", b"channel")
            .await
            .unwrap();

        let video = tokio::fs::read(&get(&pool, "sharedId").await.unwrap())
            .await
            .unwrap();
        let channel = tokio::fs::read(&get_channel(&pool, "sharedId").await.unwrap())
            .await
            .unwrap();
        assert_eq!(video, b"video");
        assert_eq!(channel, b"channel");

        let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM thumbnail_cache")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(
            n, 2,
            "video + channel entries must not overwrite each other"
        );
    }

    #[tokio::test]
    async fn put_channel_refuses_unsafe_id() {
        let pool = setup_db().await;
        let cache = TempDir::new().unwrap();
        put_channel(&pool, cache.path().to_str().unwrap(), "../escape", b"x")
            .await
            .unwrap();
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM thumbnail_cache")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(count, 0);
    }

    #[tokio::test]
    async fn get_drops_orphan_row_when_file_missing() {
        let pool = setup_db().await;
        let cache = TempDir::new().unwrap();
        put(&pool, cache.path().to_str().unwrap(), "vX", b"x")
            .await
            .unwrap();
        // Manually remove the file behind the cache's back.
        let path = thumbnail_path(cache.path().to_str().unwrap(), "vX");
        tokio::fs::remove_file(&path).await.unwrap();
        // get() must return None and clean up the row.
        assert!(get(&pool, "vX").await.is_none());
        let remaining: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM thumbnail_cache WHERE video_id = 'vX'")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(remaining, 0, "orphan row must be deleted on next get()");
    }

    #[tokio::test]
    async fn put_overwrites_prior_entry() {
        let pool = setup_db().await;
        let cache = TempDir::new().unwrap();
        put(&pool, cache.path().to_str().unwrap(), "vU", b"old")
            .await
            .unwrap();
        put(
            &pool,
            cache.path().to_str().unwrap(),
            "vU",
            b"new-and-longer",
        )
        .await
        .unwrap();

        // Exactly one row should remain (UPSERT preserved PK).
        let n: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM thumbnail_cache WHERE video_id = 'vU'")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(n, 1);

        let path = get(&pool, "vU").await.unwrap();
        let bytes = tokio::fs::read(&path).await.unwrap();
        assert_eq!(bytes, b"new-and-longer");
    }

    #[tokio::test]
    async fn cleanup_lru_evicts_oldest_until_under_cap() {
        let pool = setup_db().await;
        let cache = TempDir::new().unwrap();
        let dir = cache.path().to_str().unwrap();

        // Three entries, each ~100 bytes, total ~300 bytes.
        put(&pool, dir, "v1", &[0u8; 100]).await.unwrap();
        put(&pool, dir, "v2", &[0u8; 100]).await.unwrap();
        put(&pool, dir, "v3", &[0u8; 100]).await.unwrap();

        // Force v2 and v3 to look "older" than v1 so v1 survives. We
        // back-date their last_accessed_at; v1 stays at "now".
        sqlx::query(
            "UPDATE thumbnail_cache SET last_accessed_at = 1 WHERE video_id IN ('v2','v3')",
        )
        .execute(&pool)
        .await
        .unwrap();

        let (evicted, bytes_freed) = cleanup_lru(&pool, 150).await.unwrap();
        // Need to drop at least 150 bytes: v2 + v3 (200 bytes) cover it.
        assert_eq!(evicted, 2);
        assert_eq!(bytes_freed, 200);

        let remaining: Vec<String> =
            sqlx::query_scalar("SELECT video_id FROM thumbnail_cache ORDER BY video_id")
                .fetch_all(&pool)
                .await
                .unwrap();
        assert_eq!(remaining, vec!["v1"]);
    }

    #[tokio::test]
    async fn cleanup_lru_is_noop_when_under_cap_or_unlimited() {
        let pool = setup_db().await;
        let cache = TempDir::new().unwrap();
        put(&pool, cache.path().to_str().unwrap(), "vA", b"hello")
            .await
            .unwrap();

        // Under cap.
        let (evicted, _) = cleanup_lru(&pool, 1_000_000).await.unwrap();
        assert_eq!(evicted, 0);

        // Unlimited.
        let (evicted, _) = cleanup_lru(&pool, 0).await.unwrap();
        assert_eq!(evicted, 0);
    }

    #[tokio::test]
    async fn stats_aggregates_rows_and_bytes() {
        let pool = setup_db().await;
        let cache = TempDir::new().unwrap();
        put(&pool, cache.path().to_str().unwrap(), "vA", &[0u8; 100])
            .await
            .unwrap();
        put(&pool, cache.path().to_str().unwrap(), "vB", &[0u8; 200])
            .await
            .unwrap();
        let s = stats(&pool).await.unwrap();
        assert_eq!(s.entry_count, 2);
        assert_eq!(s.total_bytes, 300);
    }

    #[tokio::test]
    async fn configured_max_bytes_falls_back_to_default() {
        let pool = setup_db().await;
        assert_eq!(configured_max_bytes(&pool).await, DEFAULT_MAX_BYTES);

        sqlx::query("INSERT INTO app_config (key, value) VALUES (?, '1234567')")
            .bind(KEY_THUMBNAIL_CACHE_MAX_BYTES)
            .execute(&pool)
            .await
            .unwrap();
        assert_eq!(configured_max_bytes(&pool).await, 1_234_567);

        sqlx::query("UPDATE app_config SET value = 'garbage' WHERE key = ?")
            .bind(KEY_THUMBNAIL_CACHE_MAX_BYTES)
            .execute(&pool)
            .await
            .unwrap();
        assert_eq!(configured_max_bytes(&pool).await, DEFAULT_MAX_BYTES);
    }
}
