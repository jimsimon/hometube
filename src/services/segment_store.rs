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

use std::path::PathBuf;
use std::pin::Pin;
use std::task::{Context, Poll};

use bytes::{Bytes, BytesMut};
use futures_util::Stream;
use sqlx::SqlitePool;
use tracing::{debug, warn};

use crate::error::{AppError, AppResult};

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// Aligned chunk size for segment caching: 2 MiB.
pub const CHUNK_SIZE: u64 = 2 * 1024 * 1024;

// ---------------------------------------------------------------------------
// Path helpers
// ---------------------------------------------------------------------------

/// Compute the 2-character shard prefix from a video ID.
fn shard_prefix(video_id: &str) -> &str {
    if video_id.len() >= 2 {
        &video_id[..2]
    } else {
        video_id
    }
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
fn ensure_chunk_dir(cache_dir: &str, video_id: &str) -> std::io::Result<PathBuf> {
    let shard = shard_prefix(video_id);
    let dir = PathBuf::from(cache_dir).join(shard).join(video_id);
    std::fs::create_dir_all(&dir)?;
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
// DB queries
// ---------------------------------------------------------------------------

/// Returns `true` if ALL chunks covering `[byte_start, byte_end]` are present
/// in the cache.
pub async fn range_fully_cached(
    pool: &SqlitePool,
    video_id: &str,
    format_id: &str,
    byte_start: u64,
    byte_end: u64,
) -> AppResult<bool> {
    let first_chunk = chunk_index(byte_start);
    let last_chunk = chunk_index(byte_end);
    let needed = (last_chunk - first_chunk + 1) as i64;

    let count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM segment_cache \
         WHERE video_id = ? AND format_id = ? \
         AND segment_number >= ? AND segment_number <= ?",
    )
    .bind(video_id)
    .bind(format_id)
    .bind(first_chunk as i64)
    .bind(last_chunk as i64)
    .fetch_one(pool)
    .await
    .unwrap_or(0);

    Ok(count >= needed)
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
/// Uses atomic write (temp file + rename) to prevent concurrent readers
/// from observing partially-written data.
pub async fn store_chunk(
    pool: &SqlitePool,
    cache_dir: &str,
    video_id: &str,
    format_id: &str,
    chunk_num: u32,
    data: &[u8],
) -> AppResult<()> {
    // Ensure directory structure.
    ensure_chunk_dir(cache_dir, video_id)
        .map_err(|e| AppError::Other(anyhow::anyhow!("creating chunk dir: {e}")))?;

    let path = chunk_path(cache_dir, video_id, format_id, chunk_num);
    let path_str = path.to_string_lossy().to_string();
    let size = data.len() as i64;

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
        "INSERT INTO segment_cache (video_id, format_id, segment_number, file_path, file_size_bytes) \
         VALUES (?, ?, ?, ?, ?) \
         ON CONFLICT(video_id, format_id, segment_number) DO UPDATE SET \
            file_path = excluded.file_path, \
            file_size_bytes = excluded.file_size_bytes, \
            last_accessed_at = unixepoch()",
    )
    .bind(video_id)
    .bind(format_id)
    .bind(chunk_num as i64)
    .bind(&path_str)
    .bind(size)
    .execute(pool)
    .await?;

    debug!(%video_id, %format_id, chunk_num, size, "stored chunk");
    Ok(())
}

/// Read bytes from cached chunks covering `[byte_start, byte_end]`.
///
/// Caller MUST ensure all chunks exist (via `range_fully_cached`).
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
}

impl TeeStream {
    /// Wrap an upstream byte stream for tee-caching.
    ///
    /// - `range_start`: absolute byte offset of the first byte in this stream
    /// - `total_size`: total file size (for deciding whether to cache the final
    ///   partial chunk)
    pub fn new(
        inner: Pin<Box<dyn Stream<Item = Result<Bytes, std::io::Error>> + Send>>,
        pool: SqlitePool,
        cache_dir: String,
        video_id: String,
        format_id: String,
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
            range_start,
            total_size,
        ));

        Self { inner, tx }
    }
}

impl Stream for TeeStream {
    type Item = Result<Bytes, std::io::Error>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let poll = self.inner.as_mut().poll_next(cx);
        match &poll {
            Poll::Ready(Some(Ok(bytes))) => {
                // Best-effort send — if the channel is full or closed, we
                // still deliver the bytes to the client (caching is optional).
                let _ = self.tx.try_send(TeeMsg::Data(bytes.clone()));
            }
            Poll::Ready(None) => {
                // Signal end-of-stream to the writer task.
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
async fn tee_writer_task(
    mut rx: tokio::sync::mpsc::Receiver<TeeMsg>,
    pool: SqlitePool,
    cache_dir: String,
    video_id: String,
    format_id: String,
    range_start: u64,
    total_size: Option<u64>,
) {
    let mut buf = BytesMut::new();
    let mut position: u64 = range_start;

    while let Some(msg) = rx.recv().await {
        match msg {
            TeeMsg::Data(bytes) => {
                buf.extend_from_slice(&bytes);
                position += bytes.len() as u64;
                flush_complete_chunks(&pool, &cache_dir, &video_id, &format_id, &mut buf, position)
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
            if let Err(err) = store_chunk(&pool, &cache_dir, &video_id, &format_id, cn, &data).await
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
            if let Err(err) = store_chunk(pool, cache_dir, video_id, format_id, cn, &data).await {
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
}
