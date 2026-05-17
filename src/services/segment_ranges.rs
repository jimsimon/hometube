//! SegmentBase byte-range DB cache layer.
//!
//! Stores and retrieves the inclusive byte ranges of the initialization
//! segment and segment index for each `(video_id, format_id)` pair.
//! The synthesized DASH manifest uses these to emit
//! `<SegmentBase indexRange="...">` with a child
//! `<Initialization range="..."/>`.
//!
//! Ranges are populated from YouTube's innertube `/player` API response
//! at extraction time (see `services::ytdlp`). This module is purely a
//! persistence layer — it never touches the network.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use sqlx::SqlitePool;
use tracing::warn;

/// WebM Cues element ID (4 bytes, big-endian). Used by the extraction-time
/// and manifest-time validators to locate the Cues element within an
/// `indexRange` byte window.
pub const WEBM_CUES_ID: [u8; 4] = [0x1C, 0x53, 0xBB, 0x6B];

/// Inclusive byte range `[start, end]` (matching the HTTP `Range:`
/// header convention and the DASH `range="A-B"` attribute).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ByteRange {
    pub start: u64,
    pub end: u64,
}

impl ByteRange {
    /// Format as a DASH `range`/`indexRange` attribute value (`"A-B"`).
    pub fn as_dash(&self) -> String {
        format!("{}-{}", self.start, self.end)
    }
}

/// Parsed offsets we care about for SegmentBase synthesis.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BoxRanges {
    /// Byte range of the initialization data (moov/Cues). The player
    /// fetches this to read codec init data; emitted as
    /// `<Initialization range="...">`.
    pub init: ByteRange,
    /// Byte range of the segment index (sidx/Cues). The player parses
    /// this to learn segment durations + offsets; emitted as
    /// `<SegmentBase indexRange="...">`.
    pub index: ByteRange,
}

/// Look up the cached box ranges for `(video_id, format_id)`.
///
/// Returns `None` when no row exists. The lookup is keyed on the
/// `(video_id, format_id)` primary key of the `format_box_ranges`
/// table; a single row covers both `init` (moov) and `index` (sidx)
/// ranges.
pub async fn lookup(pool: &SqlitePool, video_id: &str, format_id: &str) -> Option<BoxRanges> {
    let row: Option<(i64, i64, i64, i64)> = sqlx::query_as(
        "SELECT init_start, init_end, index_start, index_end \
         FROM format_box_ranges WHERE video_id = ? AND format_id = ?",
    )
    .bind(video_id)
    .bind(format_id)
    .fetch_optional(pool)
    .await
    .ok()
    .flatten();
    row.map(|(is, ie, xs, xe)| BoxRanges {
        init: ByteRange {
            start: is as u64,
            end: ie as u64,
        },
        index: ByteRange {
            start: xs as u64,
            end: xe as u64,
        },
    })
}

/// Persist a successful range result. Idempotent — uses
/// `INSERT OR REPLACE` so a re-store quietly overwrites the prior row.
pub async fn store(pool: &SqlitePool, video_id: &str, format_id: &str, ranges: BoxRanges) {
    let result = sqlx::query(
        "INSERT OR REPLACE INTO format_box_ranges \
         (video_id, format_id, init_start, init_end, index_start, index_end) \
         VALUES (?, ?, ?, ?, ?, ?)",
    )
    .bind(video_id)
    .bind(format_id)
    .bind(ranges.init.start as i64)
    .bind(ranges.init.end as i64)
    .bind(ranges.index.start as i64)
    .bind(ranges.index.end as i64)
    .execute(pool)
    .await;
    if let Err(err) = result {
        warn!(%err, %video_id, %format_id, "persisting format_box_ranges failed");
    }
}

/// Pure cache lookup for box ranges across a list of formats.
///
/// Never touches the network. Missing entries are simply absent from
/// the result map; the caller is expected to fall back to plain
/// `<BaseURL>` rendering for those formats.
///
/// `formats` is an iterator of `(format_id, _url)` pairs; the URL is
/// ignored here and exists only to match the input shape for callers
/// that pass the same slice to both lookup and store paths.
pub async fn lookup_all(
    pool: &SqlitePool,
    video_id: &str,
    formats: &[(String, String)],
) -> HashMap<String, BoxRanges> {
    let mut out = HashMap::with_capacity(formats.len());
    for (format_id, _) in formats {
        if let Some(ranges) = lookup(pool, video_id, format_id).await {
            out.insert(format_id.clone(), ranges);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Spin up an in-memory SQLite database with all migrations
    /// applied. The pool is single-connection so the schema we create
    /// is visible to subsequent queries on the same handle.
    async fn test_pool() -> SqlitePool {
        let pool = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .unwrap();
        sqlx::migrate!("./migrations").run(&pool).await.unwrap();
        pool
    }

    fn sample_ranges() -> BoxRanges {
        BoxRanges {
            init: ByteRange {
                start: 32,
                end: 511,
            },
            index: ByteRange {
                start: 512,
                end: 4095,
            },
        }
    }

    /// `lookup_all` reports cache hits as a populated map and cache
    /// misses as absent entries — never blocks on the network.
    #[tokio::test]
    async fn lookup_all_returns_only_cached_entries() {
        let pool = test_pool().await;
        store(&pool, "vid", "137", sample_ranges()).await;

        let inputs = vec![
            ("137".into(), "https://example/137".into()),
            ("248".into(), "https://example/248".into()),
        ];
        let map = lookup_all(&pool, "vid", &inputs).await;
        assert!(map.contains_key("137"));
        assert!(!map.contains_key("248"));
    }

    /// `store` is idempotent: the unique constraint on
    /// `(video_id, format_id)` doesn't cause errors when re-storing
    /// the same row.
    #[tokio::test]
    async fn store_is_idempotent() {
        let pool = test_pool().await;
        store(&pool, "vid", "137", sample_ranges()).await;
        store(&pool, "vid", "137", sample_ranges()).await; // re-store
        let r = lookup(&pool, "vid", "137").await.expect("present");
        assert_eq!(r, sample_ranges());
    }
}
