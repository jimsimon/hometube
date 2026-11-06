//! Chunk-based segment cache: write path.
//!
//! Video format files are divided into fixed-size aligned **chunks** (2 MiB
//! each).  Only chunks that actually flow through the proxy are persisted on
//! disk.  This supports partial caching for seek/jump scenarios — a user
//! watching minutes 5–8 only caches the chunks covering those bytes.
//!
//! The sharded directory layout is:
//!
//! ```text
//! {cache_dir}/{video_id[0:2]}/{video_id}/{format_id}_{chunk:06}.chunk
//! ```
//!
//! SQLite `segment_cache` table tracks every chunk for LRU eviction.
//!
//! Every chunk also records a **fingerprint** of the upstream file it was
//! copied from (see [`format_fingerprint`]). YouTube re-encodes the files
//! behind a video's formats from time to time; the byte offsets in the
//! synthesized DASH manifest always describe the *current* file, so bytes
//! cached from an older one must never be served against it. Lookups only
//! count chunks whose fingerprint matches, and mismatched chunks are
//! purged (see [`purge_stale_chunks`]).

use std::collections::HashSet;
use std::hash::{Hash, Hasher};
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::LazyLock;
use std::task::{Context, Poll};

use bytes::{Bytes, BytesMut};
use futures_util::Stream;
use sqlx::SqlitePool;
use tracing::{debug, info, warn};

use crate::error::{AppError, AppResult};
use crate::services::ytdlp::Format;

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// Aligned chunk size for segment caching: 2 MiB.
pub const CHUNK_SIZE: u64 = 2 * 1024 * 1024;

// ---------------------------------------------------------------------------
// Path helpers
// ---------------------------------------------------------------------------

/// Compute the 2-character shard prefix from a video ID.
/// Uses `char` boundaries to avoid panicking on multi-byte UTF-8,
/// though YouTube video IDs are always ASCII in practice.
fn shard_prefix(video_id: &str) -> &str {
    let end = video_id
        .char_indices()
        .nth(2)
        .map(|(i, _)| i)
        .unwrap_or(video_id.len());
    &video_id[..end]
}

/// Compute the filesystem path for a single chunk file.
pub fn chunk_path(cache_dir: &str, video_id: &str, format_id: &str, chunk_num: u32) -> PathBuf {
    let shard = shard_prefix(video_id);
    PathBuf::from(cache_dir)
        .join(shard)
        .join(video_id)
        .join(format!("{}_{:06}.chunk", format_id, chunk_num))
}

/// Ensure the shard + video directory exists and return it.
async fn ensure_chunk_dir(cache_dir: &str, video_id: &str) -> std::io::Result<PathBuf> {
    let shard = shard_prefix(video_id);
    let dir = PathBuf::from(cache_dir).join(shard).join(video_id);
    tokio::fs::create_dir_all(&dir).await?;
    Ok(dir)
}

// ---------------------------------------------------------------------------
// Chunk math
// ---------------------------------------------------------------------------

/// Which chunk index does `byte_offset` fall in?
#[inline]
pub fn chunk_index(byte_offset: u64) -> u32 {
    (byte_offset / CHUNK_SIZE) as u32
}

/// What is the absolute byte offset of the start of chunk `n`?
#[inline]
pub fn chunk_byte_start(chunk_num: u32) -> u64 {
    chunk_num as u64 * CHUNK_SIZE
}

// ---------------------------------------------------------------------------
// Coordination between chunk writers and the stale purge
// ---------------------------------------------------------------------------

/// Number of lock stripes guarding chunk files. A collision between two
/// unrelated chunks only costs one of them a short wait, so a modest
/// count is plenty.
const CHUNK_LOCK_STRIPES: usize = 64;

/// Striped async locks keyed by `(video_id, format_id, chunk_num)`.
///
/// [`store_chunk`] and [`purge_stale_chunks`] both act on the same
/// deterministic chunk path: the writer renames a fresh copy onto it
/// and upserts the row, the purge deletes the row and unlinks the path.
/// Without mutual exclusion the purge could delete a *stale* row, then
/// unlink the *fresh* file a concurrent writer had just installed,
/// leaving a current-fingerprint row with no file behind it. Both sides
/// hold the chunk's stripe for their whole row-plus-file operation, so
/// the purge's conditional `DELETE` sees either the stale row (and
/// removes the stale file) or the fresh row (and leaves everything
/// alone). The same lock serialises two writers of one chunk, which
/// previously shared a single `.chunk.tmp` staging path.
static CHUNK_LOCKS: LazyLock<Vec<tokio::sync::Mutex<()>>> = LazyLock::new(|| {
    (0..CHUNK_LOCK_STRIPES)
        .map(|_| tokio::sync::Mutex::new(()))
        .collect()
});

fn chunk_lock(video_id: &str, format_id: &str, chunk_num: u32) -> &'static tokio::sync::Mutex<()> {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    video_id.hash(&mut hasher);
    format_id.hash(&mut hasher);
    chunk_num.hash(&mut hasher);
    &CHUNK_LOCKS[(hasher.finish() % CHUNK_LOCK_STRIPES as u64) as usize]
}

/// `(video_id, format_id)` pairs whose stale-chunk purge is currently
/// running. A player fetches segments in parallel, and until the first
/// purge has finished every one of those requests still observes the
/// stale rows; this registry makes them schedule the purge once.
static PURGES_IN_FLIGHT: LazyLock<std::sync::Mutex<HashSet<(String, String)>>> =
    LazyLock::new(|| std::sync::Mutex::new(HashSet::new()));

fn purges_in_flight() -> std::sync::MutexGuard<'static, HashSet<(String, String)>> {
    PURGES_IN_FLIGHT
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Exclusive claim on the purge slot for one format, returned by
/// [`begin_purge`]. Dropping it releases the slot so a later stale
/// detection can schedule another purge.
#[must_use = "dropping the guard immediately releases the purge slot"]
pub struct PurgeGuard {
    key: (String, String),
}

impl Drop for PurgeGuard {
    fn drop(&mut self) {
        purges_in_flight().remove(&self.key);
    }
}

/// Claim the purge slot for `(video_id, format_id)`. Returns `None` when
/// a purge for that format is already in flight.
pub fn begin_purge(video_id: &str, format_id: &str) -> Option<PurgeGuard> {
    let key = (video_id.to_string(), format_id.to_string());
    // Release the registry lock before constructing the guard: a guard
    // built while the lock is held would, if dropped, re-lock it.
    let inserted = purges_in_flight().insert(key.clone());
    inserted.then(|| PurgeGuard { key })
}

/// Run [`purge_stale_chunks`] on a background task unless one is already
/// running for this format. Returns `true` when a purge was scheduled,
/// so the caller can log the detection exactly once.
pub fn schedule_stale_purge(
    pool: &SqlitePool,
    video_id: &str,
    format_id: &str,
    fingerprint: Option<&str>,
) -> bool {
    let Some(guard) = begin_purge(video_id, format_id) else {
        return false;
    };
    let pool = pool.clone();
    let video_id = video_id.to_string();
    let format_id = format_id.to_string();
    let fingerprint = fingerprint.map(str::to_owned);
    tokio::spawn(async move {
        // Hold the slot until the purge has finished.
        let _guard = guard;
        purge_stale_chunks(&pool, &video_id, &format_id, fingerprint.as_deref()).await;
    });
    true
}

// ---------------------------------------------------------------------------
// Fingerprints
// ---------------------------------------------------------------------------

/// Identify the upstream file behind a format so cached chunks can be
/// tied to it.
///
/// YouTube's `videoplayback` URLs carry an `lmt=` (last-modified, µs)
/// query parameter that changes whenever the file is re-encoded, which
/// is the most precise signal available. We fall back to the URL's
/// `clen=` (content length) and then yt-dlp's `filesize`, either of
/// which still catches the common case where a re-encode changes the
/// file's length. Returns `None` when nothing identifying is available;
/// such formats keep the legacy "any chunk matches" behaviour.
///
/// The value is prefixed with its source (`lmt:`, `clen:`, `size:`) so
/// fingerprints derived from different signals never collide.
pub fn format_fingerprint(format: &Format) -> Option<String> {
    if let Some(url) = format.url.as_deref() {
        if let Ok(parsed) = reqwest::Url::parse(url) {
            let mut lmt = None;
            let mut clen = None;
            for (k, v) in parsed.query_pairs() {
                match &*k {
                    "lmt" if !v.is_empty() => lmt = Some(v.into_owned()),
                    "clen" if !v.is_empty() => clen = Some(v.into_owned()),
                    _ => {}
                }
            }
            if let Some(lmt) = lmt {
                return Some(format!("lmt:{lmt}"));
            }
            if let Some(clen) = clen {
                return Some(format!("clen:{clen}"));
            }
        }
    }
    format
        .filesize
        .filter(|s| *s > 0)
        .map(|s| format!("size:{s}"))
}

// ---------------------------------------------------------------------------
// DB queries
// ---------------------------------------------------------------------------

/// Result of [`range_status`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct RangeStatus {
    /// Every chunk covering the requested range is on disk **and** was
    /// copied from the file identified by the current fingerprint.
    pub fully_cached: bool,
    /// At least one chunk cached for this `(video_id, format_id)` —
    /// anywhere in the file, not just the requested range — has a
    /// fingerprint other than the current one. Includes pre-fingerprint
    /// rows (`NULL`) when the format now has a fingerprint. When set, the
    /// on-disk copy belongs to a different encode of the file and should
    /// be purged (see [`schedule_stale_purge`]).
    pub has_stale: bool,
}

/// Check whether `[byte_start, byte_end]` can be served from cache for
/// the upstream file identified by `fingerprint`, and whether any chunk
/// left over from a different upstream file exists.
///
/// `fingerprint IS ?` is SQLite's null-safe equality: a `None`
/// fingerprint matches only `NULL` rows, so formats we can't fingerprint
/// keep working against chunks that were also stored without one.
///
/// Both sub-queries are bounded by the `(video_id, format_id,
/// segment_number)` unique index, and the stale probe is an `EXISTS`
/// that stops at the first mismatching row, so this stays cheap even
/// though it runs on every ranged request.
pub async fn range_status(
    pool: &SqlitePool,
    video_id: &str,
    format_id: &str,
    fingerprint: Option<&str>,
    byte_start: u64,
    byte_end: u64,
) -> RangeStatus {
    let first_chunk = chunk_index(byte_start);
    let last_chunk = chunk_index(byte_end);
    let needed = (last_chunk - first_chunk + 1) as i64;

    let row: Result<(i64, i64), _> = sqlx::query_as(
        "SELECT \
            (SELECT COUNT(*) FROM segment_cache \
              WHERE video_id = ? AND format_id = ? AND fingerprint IS ? \
                AND segment_number >= ? AND segment_number <= ?), \
            EXISTS(SELECT 1 FROM segment_cache \
                    WHERE video_id = ? AND format_id = ? AND fingerprint IS NOT ?)",
    )
    .bind(video_id)
    .bind(format_id)
    .bind(fingerprint)
    .bind(first_chunk as i64)
    .bind(last_chunk as i64)
    .bind(video_id)
    .bind(format_id)
    .bind(fingerprint)
    .fetch_one(pool)
    .await;

    match row {
        Ok((matching, stale)) => RangeStatus {
            fully_cached: matching >= needed,
            has_stale: stale != 0,
        },
        Err(err) => {
            debug!(%err, %video_id, %format_id, "segment cache lookup failed");
            RangeStatus::default()
        }
    }
}

/// Delete every chunk of `(video_id, format_id)` whose fingerprint does
/// not match `fingerprint` — i.e. chunks copied from an older encode of
/// the file that can no longer be served against the current manifest.
///
/// Returns `(chunks_removed, bytes_freed)`. Each chunk is handled under
/// its [`chunk_lock`]: the row is deleted only if its fingerprint still
/// mismatches at that moment, and the file is unlinked only when the
/// row was ours to delete. A concurrent [`store_chunk`] for the same
/// chunk therefore either completes first (and is left alone) or waits
/// until the stale copy is fully gone before installing its fresh one.
///
/// Callers that may run concurrently should go through
/// [`schedule_stale_purge`] so the work is not duplicated.
pub async fn purge_stale_chunks(
    pool: &SqlitePool,
    video_id: &str,
    format_id: &str,
    fingerprint: Option<&str>,
) -> (u64, u64) {
    let rows: Vec<(i64, i64, String, i64)> = match sqlx::query_as(
        "SELECT id, segment_number, file_path, file_size_bytes FROM segment_cache \
         WHERE video_id = ? AND format_id = ? AND fingerprint IS NOT ?",
    )
    .bind(video_id)
    .bind(format_id)
    .bind(fingerprint)
    .fetch_all(pool)
    .await
    {
        Ok(rows) => rows,
        Err(err) => {
            warn!(%err, %video_id, %format_id, "listing stale chunks failed");
            return (0, 0);
        }
    };

    let mut removed = 0u64;
    let mut bytes = 0u64;
    for (id, segment_number, path, size) in rows {
        let chunk_num = u32::try_from(segment_number).unwrap_or(u32::MAX);
        let _chunk_guard = chunk_lock(video_id, format_id, chunk_num).lock().await;
        let deleted =
            sqlx::query("DELETE FROM segment_cache WHERE id = ? AND fingerprint IS NOT ?")
                .bind(id)
                .bind(fingerprint)
                .execute(pool)
                .await;
        match deleted {
            Ok(res) if res.rows_affected() == 1 => {
                if let Err(err) = tokio::fs::remove_file(&path).await {
                    debug!(%path, %err, "failed to remove stale chunk file");
                }
                removed += 1;
                bytes += size.max(0) as u64;
            }
            Ok(_) => {}
            Err(err) => debug!(%err, id, "failed to delete stale chunk row"),
        }
    }

    if removed > 0 {
        info!(
            %video_id,
            %format_id,
            fingerprint = fingerprint.unwrap_or("-"),
            chunks = removed,
            bytes,
            "purged stale segment cache chunks (upstream file changed)"
        );
    }
    (removed, bytes)
}

/// Touch `last_accessed_at` for chunks in a range (LRU bookkeeping).
pub async fn touch_chunks(
    pool: &SqlitePool,
    video_id: &str,
    format_id: &str,
    chunk_start: u32,
    chunk_end: u32,
) {
    let result = sqlx::query(
        "UPDATE segment_cache SET last_accessed_at = unixepoch() \
         WHERE video_id = ? AND format_id = ? \
         AND segment_number >= ? AND segment_number <= ?",
    )
    .bind(video_id)
    .bind(format_id)
    .bind(chunk_start as i64)
    .bind(chunk_end as i64)
    .execute(pool)
    .await;

    if let Err(err) = result {
        debug!(%err, "failed to touch chunk access times");
    }
}

/// Write a single chunk to disk and insert/update the DB tracking row.
///
/// `fingerprint` identifies the upstream file the bytes came from (see
/// [`format_fingerprint`]); it is what later lets [`range_status`] tell
/// a current chunk from one left over after YouTube re-encoded the file.
///
/// Uses atomic write (temp file + rename) to prevent concurrent readers
/// from observing partially-written data, and holds the chunk's
/// [`chunk_lock`] across the write and the row upsert so neither a
/// second writer nor [`purge_stale_chunks`] can interleave with it.
pub async fn store_chunk(
    pool: &SqlitePool,
    cache_dir: &str,
    video_id: &str,
    format_id: &str,
    fingerprint: Option<&str>,
    chunk_num: u32,
    data: &[u8],
) -> AppResult<()> {
    // Ensure directory structure.
    ensure_chunk_dir(cache_dir, video_id)
        .await
        .map_err(|e| AppError::Other(anyhow::anyhow!("creating chunk dir: {e}")))?;

    let path = chunk_path(cache_dir, video_id, format_id, chunk_num);
    let path_str = path.to_string_lossy().to_string();
    let size = data.len() as i64;

    let _chunk_guard = chunk_lock(video_id, format_id, chunk_num).lock().await;

    // Write to a temporary file, then atomically rename. This prevents
    // concurrent readers from seeing partial data.
    let tmp_path = path.with_extension("chunk.tmp");
    tokio::fs::write(&tmp_path, data)
        .await
        .map_err(|e| AppError::Other(anyhow::anyhow!("writing chunk tmp file: {e}")))?;
    tokio::fs::rename(&tmp_path, &path)
        .await
        .map_err(|e| AppError::Other(anyhow::anyhow!("renaming chunk file: {e}")))?;

    // Upsert the DB row.
    sqlx::query(
        "INSERT INTO segment_cache \
            (video_id, format_id, segment_number, file_path, file_size_bytes, fingerprint) \
         VALUES (?, ?, ?, ?, ?, ?) \
         ON CONFLICT(video_id, format_id, segment_number) DO UPDATE SET \
            file_path = excluded.file_path, \
            file_size_bytes = excluded.file_size_bytes, \
            fingerprint = excluded.fingerprint, \
            cached_at = unixepoch(), \
            last_accessed_at = unixepoch()",
    )
    .bind(video_id)
    .bind(format_id)
    .bind(chunk_num as i64)
    .bind(&path_str)
    .bind(size)
    .bind(fingerprint)
    .execute(pool)
    .await?;

    debug!(%video_id, %format_id, chunk_num, size, fingerprint = fingerprint.unwrap_or("-"), "stored chunk");
    Ok(())
}

/// Read bytes from cached chunks covering `[byte_start, byte_end]`.
///
/// Caller MUST ensure all chunks exist (via [`range_status`]).
/// Returns the exact bytes for the requested range, slicing within
/// the first and last chunk as needed.
pub async fn read_range_from_cache(
    cache_dir: &str,
    video_id: &str,
    format_id: &str,
    byte_start: u64,
    byte_end: u64,
) -> AppResult<Vec<u8>> {
    let first_chunk = chunk_index(byte_start);
    let last_chunk = chunk_index(byte_end);

    let mut result = Vec::with_capacity((byte_end - byte_start + 1) as usize);

    for cn in first_chunk..=last_chunk {
        let path = chunk_path(cache_dir, video_id, format_id, cn);
        let data = tokio::fs::read(&path)
            .await
            .map_err(|e| AppError::Other(anyhow::anyhow!("reading chunk {cn}: {e}")))?;

        let chunk_start_abs = chunk_byte_start(cn);

        // Where within this chunk's bytes do we start reading?
        let local_start = if cn == first_chunk {
            (byte_start - chunk_start_abs) as usize
        } else {
            0
        };

        // Where within this chunk's bytes do we stop reading?
        let local_end = if cn == last_chunk {
            (byte_end - chunk_start_abs + 1) as usize
        } else {
            data.len()
        };

        // Clamp to actual file size (last chunk of file may be smaller than CHUNK_SIZE).
        let local_end = local_end.min(data.len());

        if local_start < local_end {
            result.extend_from_slice(&data[local_start..local_end]);
        }
    }

    Ok(result)
}

/// Look up `total_bytes` for a format from `format_box_ranges`.
pub async fn get_format_total_bytes(
    pool: &SqlitePool,
    video_id: &str,
    format_id: &str,
) -> Option<u64> {
    let row: Option<(Option<i64>,)> = sqlx::query_as(
        "SELECT total_bytes FROM format_box_ranges WHERE video_id = ? AND format_id = ?",
    )
    .bind(video_id)
    .bind(format_id)
    .fetch_optional(pool)
    .await
    .ok()
    .flatten();
    row.and_then(|(tb,)| tb.map(|v| v as u64))
}

/// Store `total_bytes` for a format in `format_box_ranges`.
///
/// Only updates the `total_bytes` column on an existing row. If no row
/// exists yet (the box-range probe hasn't run), this is a no-op — the
/// probe will eventually create the row and we'll set `total_bytes` on
/// the next request. This avoids inserting dummy zero-value init/index
/// ranges that could produce invalid DASH manifests.
pub async fn set_format_total_bytes(
    pool: &SqlitePool,
    video_id: &str,
    format_id: &str,
    total: u64,
) {
    let result = sqlx::query(
        "UPDATE format_box_ranges SET total_bytes = ? WHERE video_id = ? AND format_id = ?",
    )
    .bind(total as i64)
    .bind(video_id)
    .bind(format_id)
    .execute(pool)
    .await;

    if let Err(err) = result {
        warn!(%err, %video_id, %format_id, "failed to persist total_bytes");
    }
}

// ---------------------------------------------------------------------------
// TeeStream — wraps an upstream byte stream, passes bytes through to the
// consumer (HTTP response body) while sending a copy of each chunk through
// an ordered MPSC channel to a single background task that handles disk I/O.
// This guarantees bytes are processed in order regardless of task scheduling.
// ---------------------------------------------------------------------------

/// Message sent from the stream poller to the background writer task.
enum TeeMsg {
    /// A chunk of bytes arrived from upstream.
    Data(Bytes),
    /// The upstream stream ended.
    Eof,
}

/// A `Stream<Item = Result<Bytes, std::io::Error>>` that tee-caches bytes
/// into disk chunks as they flow through.
///
/// Bytes are forwarded in order to a single background task via a bounded
/// MPSC channel, avoiding the race conditions of per-chunk spawns.
pub struct TeeStream {
    inner: Pin<Box<dyn Stream<Item = Result<Bytes, std::io::Error>> + Send>>,
    /// Sender half — dropped when the TeeStream is dropped (signals EOF
    /// if the Eof message wasn't sent explicitly due to an error/abort).
    tx: tokio::sync::mpsc::Sender<TeeMsg>,
    /// Set to `true` if any send fails (channel full/closed). Once poisoned,
    /// no further messages are sent — the writer task will see a gap in the
    /// byte stream and we must not cache any subsequent data for this request
    /// as it would be at wrong offsets.
    poisoned: bool,
}

impl TeeStream {
    /// Wrap an upstream byte stream for tee-caching.
    ///
    /// - `fingerprint`: identity of the upstream file being streamed (see
    ///   [`format_fingerprint`]), recorded on every chunk written
    /// - `range_start`: absolute byte offset of the first byte in this stream
    /// - `total_size`: total file size (for deciding whether to cache the final
    ///   partial chunk)
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        inner: Pin<Box<dyn Stream<Item = Result<Bytes, std::io::Error>> + Send>>,
        pool: SqlitePool,
        cache_dir: String,
        video_id: String,
        format_id: String,
        fingerprint: Option<String>,
        range_start: u64,
        total_size: Option<u64>,
    ) -> Self {
        // Bounded channel — backpressure if the writer falls behind.
        // 64 messages ≈ up to 64 upstream chunks buffered in the channel
        // before the stream poll blocks (each message is a Bytes reference,
        // not a full copy — the actual data lives in the shared Bytes arc).
        let (tx, rx) = tokio::sync::mpsc::channel::<TeeMsg>(64);

        // Spawn the single ordered writer task.
        tokio::spawn(tee_writer_task(
            rx,
            pool,
            cache_dir,
            video_id,
            format_id,
            fingerprint,
            range_start,
            total_size,
        ));

        Self {
            inner,
            tx,
            poisoned: false,
        }
    }
}

impl Stream for TeeStream {
    type Item = Result<Bytes, std::io::Error>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let poll = self.inner.as_mut().poll_next(cx);

        // Once poisoned, skip all channel sends — the writer task will
        // receive no more data and won't produce corrupt partial chunks.
        if self.poisoned {
            return poll;
        }

        match &poll {
            Poll::Ready(Some(Ok(bytes))) => {
                if self.tx.try_send(TeeMsg::Data(bytes.clone())).is_err() {
                    // Channel full or closed — poison the stream. Any chunks
                    // already written by the writer are valid (they were
                    // complete aligned chunks). We just stop sending more.
                    self.poisoned = true;
                    debug!("tee channel full/closed — disabling caching for this stream");
                }
            }
            Poll::Ready(None) => {
                // Signal end-of-stream to the writer task.
                // If this fails, the writer will still exit when the channel
                // is dropped (sender dropped → recv returns None).
                let _ = self.tx.try_send(TeeMsg::Eof);
            }
            Poll::Ready(Some(Err(_))) | Poll::Pending => {}
        }
        poll
    }
}

/// Single background task that processes bytes in order.
///
/// Receives `TeeMsg` items sequentially from the MPSC channel, accumulates
/// bytes in a buffer, and writes complete aligned chunks to disk.
#[allow(clippy::too_many_arguments)]
async fn tee_writer_task(
    mut rx: tokio::sync::mpsc::Receiver<TeeMsg>,
    pool: SqlitePool,
    cache_dir: String,
    video_id: String,
    format_id: String,
    fingerprint: Option<String>,
    range_start: u64,
    total_size: Option<u64>,
) {
    let fingerprint = fingerprint.as_deref();
    let mut buf = BytesMut::new();
    let mut position: u64 = range_start;

    while let Some(msg) = rx.recv().await {
        match msg {
            TeeMsg::Data(bytes) => {
                buf.extend_from_slice(&bytes);
                position += bytes.len() as u64;
                flush_complete_chunks(
                    &pool,
                    &cache_dir,
                    &video_id,
                    &format_id,
                    fingerprint,
                    &mut buf,
                    position,
                )
                .await;
            }
            TeeMsg::Eof => {
                break;
            }
        }
    }

    // Handle any remaining buffer (final partial chunk at end of file).
    if !buf.is_empty() {
        let buf_abs_start = position - buf.len() as u64;
        let cn = chunk_index(buf_abs_start);
        let cn_start = chunk_byte_start(cn);

        let is_end_of_file = total_size.is_some_and(|total| position >= total);
        let starts_at_boundary = buf_abs_start == cn_start;

        if is_end_of_file && starts_at_boundary {
            let data = buf.freeze();
            if let Err(err) = store_chunk(
                &pool,
                &cache_dir,
                &video_id,
                &format_id,
                fingerprint,
                cn,
                &data,
            )
            .await
            {
                debug!(%err, cn, "failed to cache final chunk (non-fatal)");
            }
        }
    }
}

/// Flush any complete aligned chunks from the buffer to disk.
///
/// The key invariant: we only write a chunk when:
/// 1. The buffer starts at that chunk's aligned boundary, AND
/// 2. We have accumulated CHUNK_SIZE bytes for it (a full chunk)
///
/// If the stream started mid-chunk, we discard the leading partial data
/// up to the next chunk boundary. This means the first partial chunk is
/// never cached, but all subsequent full chunks are.
async fn flush_complete_chunks(
    pool: &SqlitePool,
    cache_dir: &str,
    video_id: &str,
    format_id: &str,
    fingerprint: Option<&str>,
    buf: &mut BytesMut,
    position: u64,
) {
    loop {
        if buf.is_empty() {
            break;
        }

        let current_abs_start = position - buf.len() as u64;
        let cn = chunk_index(current_abs_start);
        let cn_start = chunk_byte_start(cn);

        if current_abs_start != cn_start {
            // We're mid-chunk. Discard bytes up to the next chunk boundary.
            let next_boundary = chunk_byte_start(cn + 1);
            let discard = ((next_boundary - current_abs_start) as usize).min(buf.len());
            let _ = buf.split_to(discard);
            continue;
        }

        // current_abs_start is chunk-aligned.
        if buf.len() >= CHUNK_SIZE as usize {
            // We have a full chunk — write it out.
            let chunk_data = buf.split_to(CHUNK_SIZE as usize);
            let data = chunk_data.freeze();
            if let Err(err) =
                store_chunk(pool, cache_dir, video_id, format_id, fingerprint, cn, &data).await
            {
                debug!(%err, cn, "failed to cache chunk (non-fatal)");
            }
        } else {
            // Not enough data yet for a full chunk — wait for more.
            break;
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chunk_index_math() {
        assert_eq!(chunk_index(0), 0);
        assert_eq!(chunk_index(CHUNK_SIZE - 1), 0);
        assert_eq!(chunk_index(CHUNK_SIZE), 1);
        assert_eq!(chunk_index(CHUNK_SIZE + 1), 1);
        assert_eq!(chunk_index(5 * CHUNK_SIZE), 5);
    }

    #[test]
    fn chunk_byte_start_math() {
        assert_eq!(chunk_byte_start(0), 0);
        assert_eq!(chunk_byte_start(1), CHUNK_SIZE);
        assert_eq!(chunk_byte_start(3), 3 * CHUNK_SIZE);
    }

    #[test]
    fn chunk_path_sharding() {
        let p = chunk_path("/data/cache", "dQw4w9WgXcQ", "137", 42);
        assert_eq!(
            p,
            PathBuf::from("/data/cache/dQ/dQw4w9WgXcQ/137_000042.chunk")
        );
    }

    #[test]
    fn chunk_path_short_video_id() {
        let p = chunk_path("/data/cache", "X", "251", 0);
        assert_eq!(p, PathBuf::from("/data/cache/X/X/251_000000.chunk"));
    }

    #[test]
    fn shard_prefix_ascii() {
        assert_eq!(shard_prefix("dQw4w9WgXcQ"), "dQ");
        assert_eq!(shard_prefix("AB"), "AB");
        assert_eq!(shard_prefix("A"), "A");
        assert_eq!(shard_prefix(""), "");
    }

    #[test]
    fn shard_prefix_multibyte_safe() {
        // Multi-byte chars: 'ñ' is 2 bytes in UTF-8, '日' is 3 bytes.
        // Should not panic — returns the first 2 *characters*.
        assert_eq!(shard_prefix("ñoño"), "ño");
        assert_eq!(shard_prefix("日本語"), "日本");
    }

    async fn test_pool() -> SqlitePool {
        let pool = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .unwrap();
        sqlx::migrate!("./migrations").run(&pool).await.unwrap();
        pool
    }

    #[tokio::test]
    async fn store_and_read_chunk_round_trip() {
        let pool = test_pool().await;
        let dir = tempfile::tempdir().unwrap();
        let cache_dir = dir.path().to_str().unwrap();
        let data = vec![42u8; CHUNK_SIZE as usize];

        store_chunk(&pool, cache_dir, "vid1", "137", None, 0, &data)
            .await
            .unwrap();

        // Verify file was written.
        let path = chunk_path(cache_dir, "vid1", "137", 0);
        assert!(path.exists());

        // Read back.
        let result = read_range_from_cache(cache_dir, "vid1", "137", 0, CHUNK_SIZE - 1)
            .await
            .unwrap();
        assert_eq!(result.len(), CHUNK_SIZE as usize);
        assert!(result.iter().all(|&b| b == 42));
    }

    #[tokio::test]
    async fn range_status_reports_correctly() {
        let pool = test_pool().await;
        let dir = tempfile::tempdir().unwrap();
        let cache_dir = dir.path().to_str().unwrap();
        let fp = Some("lmt:1");

        // No chunks cached yet.
        let st = range_status(&pool, "vid1", "137", fp, 0, CHUNK_SIZE - 1).await;
        assert_eq!(st, RangeStatus::default());

        // Store chunk 0.
        let data = vec![1u8; CHUNK_SIZE as usize];
        store_chunk(&pool, cache_dir, "vid1", "137", fp, 0, &data)
            .await
            .unwrap();

        // Now chunk 0 range is cached.
        let st = range_status(&pool, "vid1", "137", fp, 0, CHUNK_SIZE - 1).await;
        assert!(st.fully_cached);
        assert!(!st.has_stale);

        // But a range spanning chunks 0-1 is not fully cached.
        let st = range_status(&pool, "vid1", "137", fp, 0, CHUNK_SIZE * 2 - 1).await;
        assert!(!st.fully_cached);
        assert!(!st.has_stale);
    }

    #[tokio::test]
    async fn range_status_rejects_chunks_from_other_fingerprint() {
        let pool = test_pool().await;
        let dir = tempfile::tempdir().unwrap();
        let cache_dir = dir.path().to_str().unwrap();

        let data = vec![1u8; CHUNK_SIZE as usize];
        store_chunk(&pool, cache_dir, "vid1", "137", Some("lmt:old"), 0, &data)
            .await
            .unwrap();
        // A legacy row written before fingerprinting existed.
        store_chunk(&pool, cache_dir, "vid1", "137", None, 1, &data)
            .await
            .unwrap();

        // Same encode: chunk 0 hits, chunk 1 (NULL) counts as stale.
        let st = range_status(&pool, "vid1", "137", Some("lmt:old"), 0, CHUNK_SIZE - 1).await;
        assert!(st.fully_cached);
        assert!(st.has_stale);

        // New encode: nothing matches, both chunks are stale.
        let st = range_status(&pool, "vid1", "137", Some("lmt:new"), 0, CHUNK_SIZE - 1).await;
        assert!(!st.fully_cached);
        assert!(st.has_stale);

        // No fingerprint at all: only the NULL row matches.
        let st = range_status(&pool, "vid1", "137", None, CHUNK_SIZE, CHUNK_SIZE * 2 - 1).await;
        assert!(st.fully_cached);
        assert!(st.has_stale);
        let st = range_status(&pool, "vid1", "137", None, 0, CHUNK_SIZE - 1).await;
        assert!(!st.fully_cached);
    }

    #[tokio::test]
    async fn purge_stale_chunks_removes_only_mismatched() {
        let pool = test_pool().await;
        let dir = tempfile::tempdir().unwrap();
        let cache_dir = dir.path().to_str().unwrap();

        let data = vec![1u8; CHUNK_SIZE as usize];
        store_chunk(&pool, cache_dir, "vid1", "137", Some("lmt:old"), 0, &data)
            .await
            .unwrap();
        store_chunk(&pool, cache_dir, "vid1", "137", None, 1, &data)
            .await
            .unwrap();
        store_chunk(&pool, cache_dir, "vid1", "137", Some("lmt:new"), 2, &data)
            .await
            .unwrap();
        // A different format of the same video must be untouched.
        store_chunk(&pool, cache_dir, "vid1", "251", Some("lmt:old"), 0, &data)
            .await
            .unwrap();

        let (removed, bytes) = purge_stale_chunks(&pool, "vid1", "137", Some("lmt:new")).await;
        assert_eq!(removed, 2);
        assert_eq!(bytes, CHUNK_SIZE * 2);

        assert!(!chunk_path(cache_dir, "vid1", "137", 0).exists());
        assert!(!chunk_path(cache_dir, "vid1", "137", 1).exists());
        assert!(chunk_path(cache_dir, "vid1", "137", 2).exists());
        assert!(chunk_path(cache_dir, "vid1", "251", 0).exists());

        let remaining: Vec<(String, i64, Option<String>)> = sqlx::query_as(
            "SELECT format_id, segment_number, fingerprint FROM segment_cache \
             WHERE video_id = 'vid1' ORDER BY format_id, segment_number",
        )
        .fetch_all(&pool)
        .await
        .unwrap();
        assert_eq!(
            remaining,
            vec![
                ("137".to_string(), 2, Some("lmt:new".to_string())),
                ("251".to_string(), 0, Some("lmt:old".to_string())),
            ]
        );

        // Idempotent.
        let (removed, _) = purge_stale_chunks(&pool, "vid1", "137", Some("lmt:new")).await;
        assert_eq!(removed, 0);
    }

    /// A purge racing a writer for the same chunk must never leave a
    /// current-fingerprint row whose file has been unlinked. Repeats the
    /// race many times with both operations started concurrently; the
    /// per-chunk lock makes every interleaving end in a consistent state.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn purge_racing_store_never_strands_a_fresh_chunk() {
        let pool = test_pool().await;
        let dir = tempfile::tempdir().unwrap();
        let cache_dir = dir.path().to_str().unwrap();
        let stale = vec![0xAAu8; 256];
        let fresh = vec![0xBBu8; 256];

        for _ in 0..40 {
            store_chunk(&pool, cache_dir, "vid1", "137", Some("lmt:old"), 0, &stale)
                .await
                .unwrap();

            let (purge, store) = tokio::join!(
                purge_stale_chunks(&pool, "vid1", "137", Some("lmt:new")),
                store_chunk(&pool, cache_dir, "vid1", "137", Some("lmt:new"), 0, &fresh),
            );
            store.unwrap();
            // Either the purge got to the stale row first (1 removed) or
            // the writer replaced it first and the purge found nothing to
            // do (0 removed). Both are fine; what must never happen is the
            // fresh row surviving without its file.
            assert!(purge.0 <= 1, "at most the stale copy is purged");

            let row: (Option<String>,) = sqlx::query_as(
                "SELECT fingerprint FROM segment_cache \
                 WHERE video_id = 'vid1' AND format_id = '137' AND segment_number = 0",
            )
            .fetch_one(&pool)
            .await
            .unwrap();
            assert_eq!(row.0.as_deref(), Some("lmt:new"));
            let on_disk = tokio::fs::read(chunk_path(cache_dir, "vid1", "137", 0))
                .await
                .expect("fresh chunk file must survive the purge");
            assert_eq!(on_disk, fresh);
        }
    }

    #[test]
    fn begin_purge_is_exclusive_per_format_until_released() {
        let first = begin_purge("vidX", "137").expect("slot is free");
        assert!(begin_purge("vidX", "137").is_none(), "already in flight");
        // Other formats and videos are independent.
        let other = begin_purge("vidX", "251").expect("different format");
        assert!(begin_purge("vidY", "137").is_some());
        drop(other);
        drop(first);
        assert!(begin_purge("vidX", "137").is_some(), "released on drop");
    }

    /// Parallel segment requests that all see the stale rows schedule a
    /// single purge; once it has run the slot is free again.
    #[tokio::test]
    async fn schedule_stale_purge_runs_once_per_format() {
        let pool = test_pool().await;
        let dir = tempfile::tempdir().unwrap();
        let cache_dir = dir.path().to_str().unwrap();
        let data = vec![1u8; 64];
        store_chunk(&pool, cache_dir, "vidS", "137", Some("lmt:old"), 0, &data)
            .await
            .unwrap();
        store_chunk(&pool, cache_dir, "vidS", "137", Some("lmt:old"), 1, &data)
            .await
            .unwrap();

        // The current-thread runtime does not poll the spawned purge
        // until this test yields, so the second call deterministically
        // observes the first one still in flight.
        assert!(schedule_stale_purge(&pool, "vidS", "137", Some("lmt:new")));
        assert!(!schedule_stale_purge(&pool, "vidS", "137", Some("lmt:new")));

        for _ in 0..200 {
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            let left: i64 =
                sqlx::query_scalar("SELECT COUNT(*) FROM segment_cache WHERE video_id = 'vidS'")
                    .fetch_one(&pool)
                    .await
                    .unwrap();
            if left == 0 && begin_purge("vidS", "137").is_some() {
                return;
            }
        }
        panic!("purge did not complete and release its slot");
    }

    fn format_with(url: Option<&str>, filesize: Option<i64>) -> Format {
        Format {
            format_id: "137".into(),
            ext: None,
            height: None,
            width: None,
            tbr: None,
            vbr: None,
            abr: None,
            fps: None,
            vcodec: None,
            acodec: None,
            filesize,
            url: url.map(String::from),
            protocol: None,
            language: None,
            language_preference: None,
            format_note: None,
        }
    }

    #[test]
    fn format_fingerprint_prefers_lmt_then_clen_then_filesize() {
        let f = format_with(
            Some("https://rr1.googlevideo.com/videoplayback?expire=1&clen=123&lmt=1758150000000000&sig=x"),
            Some(123),
        );
        assert_eq!(
            format_fingerprint(&f).as_deref(),
            Some("lmt:1758150000000000")
        );

        let f = format_with(
            Some("https://rr1.googlevideo.com/videoplayback?clen=123&sig=x"),
            Some(999),
        );
        assert_eq!(format_fingerprint(&f).as_deref(), Some("clen:123"));

        let f = format_with(Some("https://example.com/fake"), Some(999));
        assert_eq!(format_fingerprint(&f).as_deref(), Some("size:999"));

        // Negative/zero filesize sentinels and missing data yield None.
        let f = format_with(Some("https://example.com/fake"), Some(-1));
        assert_eq!(format_fingerprint(&f), None);
        let f = format_with(None, None);
        assert_eq!(format_fingerprint(&f), None);

        // An unparsable URL falls back to filesize rather than failing.
        let f = format_with(Some("not a url"), Some(5));
        assert_eq!(format_fingerprint(&f).as_deref(), Some("size:5"));
    }

    #[test]
    fn format_fingerprint_changes_when_file_is_regenerated() {
        let before = format_with(
            Some("https://x/videoplayback?clen=100&lmt=1700000000000000"),
            Some(100),
        );
        let after = format_with(
            Some("https://x/videoplayback?clen=100&lmt=1758150000000000"),
            Some(100),
        );
        assert_ne!(format_fingerprint(&before), format_fingerprint(&after));
    }

    #[tokio::test]
    async fn read_range_slices_within_chunks() {
        let pool = test_pool().await;
        let dir = tempfile::tempdir().unwrap();
        let cache_dir = dir.path().to_str().unwrap();

        // Store a chunk with sequential bytes.
        let data: Vec<u8> = (0..CHUNK_SIZE).map(|i| (i % 256) as u8).collect();
        store_chunk(&pool, cache_dir, "vid1", "137", None, 0, &data)
            .await
            .unwrap();

        // Read a slice from the middle.
        let result = read_range_from_cache(cache_dir, "vid1", "137", 100, 199)
            .await
            .unwrap();
        assert_eq!(result.len(), 100);
        assert_eq!(result[0], 100);
        assert_eq!(result[99], 199);
    }

    #[tokio::test]
    async fn touch_chunks_updates_last_accessed() {
        let pool = test_pool().await;
        let dir = tempfile::tempdir().unwrap();
        let cache_dir = dir.path().to_str().unwrap();

        let data = vec![0u8; CHUNK_SIZE as usize];
        store_chunk(&pool, cache_dir, "vid1", "137", None, 0, &data)
            .await
            .unwrap();

        // Touch the chunk.
        touch_chunks(&pool, "vid1", "137", 0, 0).await;

        // Verify last_accessed_at was updated (it should be recent).
        let row: (i64,) =
            sqlx::query_as("SELECT last_accessed_at FROM segment_cache WHERE video_id = 'vid1'")
                .fetch_one(&pool)
                .await
                .unwrap();
        let now = chrono::Utc::now().timestamp();
        assert!((now - row.0).abs() < 5);
    }

    #[tokio::test]
    async fn store_chunk_atomic_write() {
        let pool = test_pool().await;
        let dir = tempfile::tempdir().unwrap();
        let cache_dir = dir.path().to_str().unwrap();

        let data = vec![99u8; CHUNK_SIZE as usize];
        store_chunk(&pool, cache_dir, "vid1", "137", None, 0, &data)
            .await
            .unwrap();

        // Verify no .tmp file remains.
        let tmp_path = chunk_path(cache_dir, "vid1", "137", 0).with_extension("chunk.tmp");
        assert!(!tmp_path.exists());
    }

    #[tokio::test]
    async fn get_and_set_format_total_bytes() {
        let pool = test_pool().await;

        // No row exists — returns None.
        assert_eq!(get_format_total_bytes(&pool, "vid1", "137").await, None);

        // Set without existing row — no-op (UPDATE on non-existent row).
        set_format_total_bytes(&pool, "vid1", "137", 5000).await;
        assert_eq!(get_format_total_bytes(&pool, "vid1", "137").await, None);

        // Insert a real row first (simulating the box-range probe).
        sqlx::query(
            "INSERT INTO format_box_ranges (video_id, format_id, init_start, init_end, index_start, index_end) \
             VALUES ('vid1', '137', 0, 511, 512, 4095)",
        )
        .execute(&pool)
        .await
        .unwrap();

        // Now set_format_total_bytes should work.
        set_format_total_bytes(&pool, "vid1", "137", 12345678).await;
        assert_eq!(
            get_format_total_bytes(&pool, "vid1", "137").await,
            Some(12345678)
        );
    }

    #[tokio::test]
    async fn flush_complete_chunks_aligned_start() {
        let pool = test_pool().await;
        let dir = tempfile::tempdir().unwrap();
        let cache_dir = dir.path().to_str().unwrap();

        // Simulate receiving 3 MiB of data starting at offset 0.
        let mut buf = BytesMut::from(&vec![7u8; (CHUNK_SIZE * 3 / 2) as usize][..]);
        let position = CHUNK_SIZE * 3 / 2; // 3 MiB received

        flush_complete_chunks(&pool, cache_dir, "vid1", "137", None, &mut buf, position).await;

        // Should have written 1 full chunk (2 MiB), leaving 1 MiB in buffer.
        assert_eq!(buf.len(), (CHUNK_SIZE / 2) as usize);

        // Chunk 0 should exist on disk.
        let path = chunk_path(cache_dir, "vid1", "137", 0);
        assert!(path.exists());
        let on_disk = tokio::fs::read(&path).await.unwrap();
        assert_eq!(on_disk.len(), CHUNK_SIZE as usize);
    }

    #[tokio::test]
    async fn flush_complete_chunks_mid_chunk_start_discards() {
        let pool = test_pool().await;
        let dir = tempfile::tempdir().unwrap();
        let cache_dir = dir.path().to_str().unwrap();

        // Simulate starting at byte offset 1 MiB (mid-chunk 0).
        // Buffer has 3 MiB of data, position = 1 MiB + 3 MiB = 4 MiB.
        let data_len = (CHUNK_SIZE * 3 / 2) as usize; // 3 MiB
        let mut buf = BytesMut::from(&vec![5u8; data_len][..]);
        let start_offset = CHUNK_SIZE / 2; // 1 MiB into chunk 0
        let position = start_offset + data_len as u64;

        flush_complete_chunks(&pool, cache_dir, "vid1", "137", None, &mut buf, position).await;

        // The first 1 MiB should be discarded (tail of chunk 0).
        // Then we have 2 MiB aligned at chunk 1 boundary → written.
        // Remaining: 0 bytes.
        assert_eq!(buf.len(), 0);

        // Chunk 0 should NOT exist (we started mid-chunk).
        let path0 = chunk_path(cache_dir, "vid1", "137", 0);
        assert!(!path0.exists());

        // Chunk 1 should exist.
        let path1 = chunk_path(cache_dir, "vid1", "137", 1);
        assert!(path1.exists());
    }
}
