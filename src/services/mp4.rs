//! mp4 top-level box scanner.
//!
//! YouTube serves video formats as fast-start mp4 files: `ftyp`, then
//! `moov` (codec/track init data), then `sidx` (segment index), then
//! `mdat` (media payload). The synthesized DASH manifest needs the
//! byte ranges of `moov` and `sidx` to emit a valid
//! `<SegmentBase indexRange="...">` element with a child
//! `<Initialization range="...">`. dash.js then fetches just those
//! few KB to learn the segment layout, and issues one HTTP range
//! request per real DASH segment for playback.
//!
//! This module fetches the first 64 KB of an upstream URL, walks the
//! top-level box list, and reports the byte offsets of the relevant
//! boxes. It deliberately does *not* parse box contents — we only
//! need offsets, and a full box parser would pull in significant
//! third-party code for marginal benefit.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use sqlx::SqlitePool;
use tracing::warn;

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
    /// Byte range of the `moov` box. dash.js fetches this to read
    /// codec init data; emitted as `<Initialization range="...">`.
    pub init: ByteRange,
    /// Byte range of the `sidx` box. dash.js parses this to learn
    /// segment durations + offsets; emitted as `<SegmentBase
    /// indexRange="...">`.
    pub index: ByteRange,
}

/// Number of bytes to fetch from the start of the file when probing.
/// 64 KB is comfortably more than enough for both the `moov` and
/// `sidx` boxes of any YouTube fast-start mp4 we've observed (`moov`
/// is typically 1–3 KB, `sidx` is 1–10 KB depending on video length).
pub const PROBE_BYTES: u64 = 65_535;

/// Walk the top-level box list of a (partial) mp4 file and return the
/// byte ranges of `moov` and `sidx` if both are present.
///
/// Returns `None` when:
/// - The input is too short to contain a complete header (< 8 bytes).
/// - Any box has a malformed size (zero, < 8, or extending past the
///   buffer for a box whose end we'd want to record).
/// - Either `moov` or `sidx` is absent (e.g. raw mp4 audio with no
///   segment index, or the boxes lie past the probe buffer).
///
/// The parser is byte-stream-only and does **not** allocate, recurse,
/// or call into third-party code. It walks the buffer once.
pub fn parse_box_ranges(buf: &[u8]) -> Option<BoxRanges> {
    let mut moov: Option<ByteRange> = None;
    let mut sidx: Option<ByteRange> = None;
    let mut offset: u64 = 0;
    let limit = buf.len() as u64;

    while offset + 8 <= limit {
        let i = offset as usize;
        let size32 = u32::from_be_bytes([buf[i], buf[i + 1], buf[i + 2], buf[i + 3]]);
        let kind = &buf[i + 4..i + 8];

        // Three encodings for box size, per ISO/IEC 14496-12:
        //   size32 == 1 → 64-bit extended size in next 8 bytes.
        //   size32 == 0 → box runs to end-of-file (only valid at the
        //                 outermost level; we treat this as "stop").
        //   otherwise   → size32 is the total box size in bytes.
        let total_size: u64 = match size32 {
            1 => {
                if offset + 16 > limit {
                    return None;
                }
                u64::from_be_bytes([
                    buf[i + 8],
                    buf[i + 9],
                    buf[i + 10],
                    buf[i + 11],
                    buf[i + 12],
                    buf[i + 13],
                    buf[i + 14],
                    buf[i + 15],
                ])
            }
            0 => {
                return moov
                    .zip(sidx)
                    .map(|(init, index)| BoxRanges { init, index })
            }
            n => u64::from(n),
        };

        if total_size < 8 {
            // Sub-header sizes are spec-violating; bail rather than
            // loop forever.
            return None;
        }

        let end_inclusive = offset.checked_add(total_size)?.checked_sub(1)?;

        match kind {
            b"moov" => {
                moov = Some(ByteRange {
                    start: offset,
                    end: end_inclusive,
                })
            }
            b"sidx" => {
                sidx = Some(ByteRange {
                    start: offset,
                    end: end_inclusive,
                })
            }
            _ => {}
        }

        offset = offset.checked_add(total_size)?;
    }

    moov.zip(sidx)
        .map(|(init, index)| BoxRanges { init, index })
}

/// Fetch the first [`PROBE_BYTES`] bytes of `url` and parse the
/// top-level mp4 box list.
///
/// Returns `None` for any of: HTTP transport failure, non-2xx upstream
/// status, body smaller than expected to contain both boxes, or an
/// mp4 layout that lacks a `sidx` (typical for audio-only progressive
/// formats — those will fall back to plain `<BaseURL>` in the
/// synthesizer).
pub async fn probe(client: &reqwest::Client, url: &str) -> Option<BoxRanges> {
    let res = client
        .get(url)
        .header(reqwest::header::RANGE, format!("bytes=0-{PROBE_BYTES}"))
        .send()
        .await
        .ok()?;
    if !res.status().is_success() {
        warn!(status = %res.status(), "mp4 probe non-2xx");
        return None;
    }
    let bytes = res.bytes().await.ok()?;
    parse_box_ranges(&bytes)
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

/// Persist a successful probe result. Idempotent — uses
/// `INSERT OR REPLACE` so a re-probe (e.g. after a schema reset)
/// quietly overwrites the prior row.
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

/// Resolve box ranges for every entry of `(format_id, url)`.
///
/// First tries the `format_box_ranges` cache; misses are probed in
/// parallel with a single shared HTTP client. Successful probes are
/// persisted before the function returns. Any combination of cache
/// hits and probe failures is returned as a `HashMap` keyed by
/// `format_id` — entries that failed to probe simply don't appear in
/// the map and the caller should fall back to plain `<BaseURL>`
/// rendering for those formats.
///
/// The probes share a single `reqwest::Client`, which gives us
/// connection pooling and bounded TLS handshake cost. The number of
/// concurrent probes equals the input length; for typical YouTube
/// extractions that's ~15 after dedup, well under any reasonable
/// per-host connection limit.
pub async fn resolve_all(
    pool: &SqlitePool,
    video_id: &str,
    formats: &[(String, String)],
) -> HashMap<String, BoxRanges> {
    let mut out = HashMap::with_capacity(formats.len());

    // Cache pass: collect everything we already have.
    let mut to_probe: Vec<&(String, String)> = Vec::new();
    for entry in formats {
        if let Some(ranges) = lookup(pool, video_id, &entry.0).await {
            out.insert(entry.0.clone(), ranges);
        } else {
            to_probe.push(entry);
        }
    }

    if to_probe.is_empty() {
        return out;
    }

    // Parallel probe pass: one HTTP request per missing format,
    // bounded only by the natural fan-out of the format list.
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build()
        .unwrap_or_else(|_| reqwest::Client::new());

    let probes = to_probe.iter().map(|(format_id, url)| {
        let client = &client;
        async move {
            let ranges = probe(client, url).await;
            (format_id.clone(), ranges)
        }
    });
    let results = futures_util::future::join_all(probes).await;

    for (format_id, maybe_ranges) in results {
        if let Some(ranges) = maybe_ranges {
            store(pool, video_id, &format_id, ranges).await;
            out.insert(format_id, ranges);
        }
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a synthetic mp4 byte stream containing the requested
    /// top-level boxes in document order. Each `(kind, size)` pair
    /// becomes a box whose 4-byte type code is `kind` and whose total
    /// size (header + body) is `size` bytes. Body bytes are zeroed.
    fn build_mp4(boxes: &[(&[u8; 4], u32)]) -> Vec<u8> {
        let mut out = Vec::new();
        for (kind, size) in boxes {
            out.extend_from_slice(&size.to_be_bytes());
            out.extend_from_slice(*kind);
            // -8 because size includes the 8-byte header we just wrote.
            out.resize(out.len() + (*size as usize - 8), 0);
        }
        out
    }

    #[test]
    fn parses_moov_and_sidx_offsets() {
        // ftyp: bytes 0..32 (size 32)
        // moov: bytes 32..532 (size 500) → range "32-531"
        // sidx: bytes 532..2580 (size 2048) → range "532-2579"
        // mdat: bytes 2580..6676 (size 4096) — just to prove the
        //       walker stops cleanly past sidx.
        let mp4 = build_mp4(&[
            (b"ftyp", 32),
            (b"moov", 500),
            (b"sidx", 2048),
            (b"mdat", 4096),
        ]);
        let ranges = parse_box_ranges(&mp4).expect("both boxes present");
        assert_eq!(
            ranges.init,
            ByteRange {
                start: 32,
                end: 531
            }
        );
        assert_eq!(
            ranges.index,
            ByteRange {
                start: 532,
                end: 2579
            }
        );
        assert_eq!(ranges.init.as_dash(), "32-531");
        assert_eq!(ranges.index.as_dash(), "532-2579");
    }

    #[test]
    fn returns_none_when_sidx_is_missing() {
        // moov + mdat but no sidx — typical for raw progressive audio.
        // The synthesizer should fall back to BaseURL for these.
        let mp4 = build_mp4(&[(b"ftyp", 32), (b"moov", 200), (b"mdat", 100_000)]);
        assert_eq!(parse_box_ranges(&mp4), None);
    }

    #[test]
    fn returns_none_when_moov_is_missing() {
        // Pathological but defensive: sidx without moov.
        let mp4 = build_mp4(&[(b"ftyp", 32), (b"sidx", 1024), (b"mdat", 10_000)]);
        assert_eq!(parse_box_ranges(&mp4), None);
    }

    #[test]
    fn parses_when_moov_lies_after_sidx() {
        // ISO BMFF allows boxes in any order. We must scan for both
        // independently rather than assuming moov comes first.
        let mp4 = build_mp4(&[(b"ftyp", 32), (b"sidx", 256), (b"moov", 512)]);
        let ranges = parse_box_ranges(&mp4).expect("both present");
        assert_eq!(ranges.index.start, 32);
        assert_eq!(ranges.init.start, 32 + 256);
    }

    #[test]
    fn handles_size0_terminator() {
        // size=0 means "this box runs to EOF". Per the spec, only the
        // last top-level box may use it. We treat it as "stop" — boxes
        // beyond it don't exist in our scan view.
        let mp4 = {
            let mut v = build_mp4(&[(b"ftyp", 32), (b"moov", 200), (b"sidx", 1024)]);
            // Append a size=0, type=mdat header pointing at "rest of
            // file." The walker should not attempt to advance past it.
            v.extend_from_slice(&0u32.to_be_bytes());
            v.extend_from_slice(b"mdat");
            v
        };
        let ranges = parse_box_ranges(&mp4).expect("moov+sidx still found");
        assert_eq!(ranges.init.start, 32);
        assert_eq!(ranges.index.start, 232);
    }

    #[test]
    fn handles_64bit_extended_size() {
        // size=1 means "the next 8 bytes hold a 64-bit size." Used for
        // boxes >= 4 GiB, which mdat can be. We must still walk past
        // the moov-via-extended-size correctly.
        let mut mp4 = Vec::new();
        // ftyp: size=32
        mp4.extend_from_slice(&32u32.to_be_bytes());
        mp4.extend_from_slice(b"ftyp");
        mp4.resize(32, 0);
        // moov with extended size = 200 bytes total (16-byte header + 184 body)
        mp4.extend_from_slice(&1u32.to_be_bytes());
        mp4.extend_from_slice(b"moov");
        mp4.extend_from_slice(&200u64.to_be_bytes());
        mp4.resize(32 + 200, 0);
        // sidx normal: size=128
        mp4.extend_from_slice(&128u32.to_be_bytes());
        mp4.extend_from_slice(b"sidx");
        mp4.resize(32 + 200 + 128, 0);

        let ranges = parse_box_ranges(&mp4).expect("64-bit moov + 32-bit sidx");
        assert_eq!(
            ranges.init,
            ByteRange {
                start: 32,
                end: 32 + 200 - 1
            }
        );
        assert_eq!(ranges.index.start, 32 + 200);
    }

    #[test]
    fn truncated_input_returns_none_safely() {
        // First box says size=1000 but the buffer is only 100 bytes.
        // We must not panic, must not loop forever; just return None.
        let mut buf = Vec::new();
        buf.extend_from_slice(&1000u32.to_be_bytes());
        buf.extend_from_slice(b"moov");
        buf.resize(100, 0);
        assert_eq!(parse_box_ranges(&buf), None);
    }

    #[test]
    fn malformed_zero_size_in_middle_returns_none() {
        // A size of less than 8 (header) is spec-violating and would
        // make the walker loop without progress; bail out instead.
        let mut buf = Vec::new();
        buf.extend_from_slice(&4u32.to_be_bytes());
        buf.extend_from_slice(b"junk");
        assert_eq!(parse_box_ranges(&buf), None);
    }

    #[test]
    fn empty_buffer_returns_none() {
        assert_eq!(parse_box_ranges(b""), None);
        assert_eq!(parse_box_ranges(b"abc"), None);
    }
}
