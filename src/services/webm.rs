//! webm/EBML element scanner.
//!
//! YouTube serves vp9 video and opus audio as `webm_dash` files. Like
//! the mp4 fast-start layout in [`crate::services::mp4`], every
//! webm we've observed places the EBML header, Segment header,
//! SeekHead, Info, Tracks, then Cues all before the first Cluster —
//! comfortably within the first 64 KB of the file.
//!
//! For DASH playback dash.js needs:
//! - **Initialization range**: bytes from start of file through end of
//!   the Tracks element (everything before the first Cluster). This
//!   is the EBML Initialization Segment per the WebM-DASH spec.
//! - **Index range**: byte range of the Cues element. Cues plays the
//!   role of mp4's `sidx` — it lists CuePoints that map presentation
//!   times to byte offsets of Clusters.
//!
//! Both ranges are emitted in the synthesized manifest as
//! `<SegmentBase indexRange>` + `<Initialization range>`. dash.js
//! fetches just those few KB to learn segment timing, then issues
//! per-Cluster byte-range requests against the BaseURL — exactly the
//! same shape as the mp4 path.
//!
//! The parser is byte-stream-only and does not allocate, recurse, or
//! call into third-party EBML/matroska libraries. It walks the buffer
//! once.
//!
//! Storage and the in-flight dedupe set are shared with
//! [`crate::services::mp4`]: both containers' results live in the
//! same `format_box_ranges` SQLite table, since the schema only
//! cares about `(init, index)` byte offsets, not the container kind.

use sqlx::SqlitePool;
use tracing::warn;

use crate::services::mp4::{BoxRanges, ByteRange};

/// Number of bytes to fetch from the start of the file when probing.
/// 64 KB is enough for every YouTube `webm_dash` we've observed —
/// SeekHead + Info + Tracks together total under 256 bytes, and the
/// Cues element is typically 2–8 KB depending on video length.
pub const PROBE_BYTES: u64 = 65_535;

/// EBML element IDs we care about. EBML IDs are variable-length
/// big-endian integers but for our purposes they fit in `u32` (every
/// matroska/webm class-A and class-B ID does).
mod id {
    pub const SEGMENT: u32 = 0x18538067;
    pub const SEEK_HEAD: u32 = 0x114D9B74;
    pub const INFO: u32 = 0x1549A966;
    pub const TRACKS: u32 = 0x1654AE6B;
    pub const CUES: u32 = 0x1C53BB6B;
    pub const CLUSTER: u32 = 0x1F43B675;
}

/// Decode an EBML variable-length integer ("VINT") starting at
/// `buf[off]`. Returns `(value, byte_count)`.
///
/// Two flavours, controlled by `mask_marker`:
/// - **`mask_marker = false`** (used for element IDs): the leading
///   "length marker" bit is preserved — IDs include their length
///   prefix as part of the canonical bit pattern.
/// - **`mask_marker = true`** (used for sizes): the marker bit is
///   cleared so the returned value is the numeric size only.
///
/// Returns `None` for the all-zero unknown-size sentinel
/// (`0xFF`/`0x7F FF`/...) and for malformed inputs (zero leading
/// byte, length running past the buffer, or length > 8).
fn read_vint(buf: &[u8], off: usize, mask_marker: bool) -> Option<(u64, usize)> {
    if off >= buf.len() {
        return None;
    }
    let b0 = buf[off];
    if b0 == 0 {
        return None;
    }
    // Length = position of the highest set bit, counted from MSB.
    let length = (b0.leading_zeros() as usize) + 1;
    if length > 8 || off + length > buf.len() {
        return None;
    }

    let mut val: u64 = if mask_marker {
        // Clear the length-marker bit (the highest set bit).
        u64::from(b0 & ((0x80u8 >> (length - 1)) - 1))
    } else {
        u64::from(b0)
    };
    for i in 1..length {
        val = (val << 8) | u64::from(buf[off + i]);
    }
    Some((val, length))
}

/// Read an EBML element ID at `buf[off]`. Returns `(id, header_len)`.
/// IDs are read with the length-marker bit preserved so identity
/// comparisons against compile-time constants like [`id::CUES`] work
/// directly.
fn read_id(buf: &[u8], off: usize) -> Option<(u32, usize)> {
    let (val, n) = read_vint(buf, off, /* mask_marker */ false)?;
    // Class-A through class-D EBML IDs all fit in u32.
    Some((val as u32, n))
}

/// Read an EBML element size at `buf[off]`. Returns `(size, header_len)`.
fn read_size(buf: &[u8], off: usize) -> Option<(u64, usize)> {
    read_vint(buf, off, /* mask_marker */ true)
}

/// Walk a (partial) webm/matroska byte stream and return the byte
/// ranges of the Initialization Segment (everything up to the first
/// Cluster) and the Cues element.
///
/// Returns `None` when:
/// - The buffer doesn't start with an EBML header followed by a
///   Segment element.
/// - Either the Cues element or the first Cluster is missing or lies
///   past the probe buffer.
/// - Any element header is malformed (zero size byte, unknown-size
///   sentinel inside Segment, length running past the buffer).
///
/// The walker descends one level only — into the Segment body — and
/// scans its top-level children sequentially. That's sufficient for
/// every YouTube `webm_dash` we've observed; we don't follow SeekHead
/// pointers.
pub fn parse_box_ranges(buf: &[u8]) -> Option<BoxRanges> {
    let limit = buf.len();

    // Step 1: skip the EBML header.
    let (ebml_id, ebml_id_len) = read_id(buf, 0)?;
    if ebml_id != 0x1A45DFA3 {
        return None;
    }
    let (ebml_size, ebml_size_len) = read_size(buf, ebml_id_len)?;
    let segment_header_off = ebml_id_len + ebml_size_len + ebml_size as usize;
    if segment_header_off >= limit {
        return None;
    }

    // Step 2: open the Segment element.
    let (seg_id, seg_id_len) = read_id(buf, segment_header_off)?;
    if seg_id != id::SEGMENT {
        return None;
    }
    let (_seg_size, seg_size_len) = read_size(buf, segment_header_off + seg_id_len)?;
    let segment_body_off = segment_header_off + seg_id_len + seg_size_len;

    // Step 3: walk Segment children to locate Cues + first Cluster.
    let mut cues: Option<ByteRange> = None;
    let mut first_cluster_start: Option<u64> = None;
    let mut off = segment_body_off;
    while off + 2 <= limit && first_cluster_start.is_none() {
        let (cid, cid_len) = read_id(buf, off)?;
        let (csize, csize_len) = read_size(buf, off + cid_len)?;
        let body_start = off + cid_len + csize_len;
        let total = cid_len + csize_len + csize as usize;
        let end_inclusive = (off as u64).checked_add(total as u64)?.checked_sub(1)?;

        match cid {
            id::CUES => {
                // Cues spans from its own start through end of body.
                cues = Some(ByteRange {
                    start: off as u64,
                    end: end_inclusive,
                });
            }
            id::CLUSTER => {
                first_cluster_start = Some(off as u64);
                break;
            }
            id::SEEK_HEAD | id::INFO | id::TRACKS => {
                // Header-y Segment children — skip. They contribute
                // to the init range but we compute that as
                // 0..first_cluster-1, so we don't need their offsets.
                let _ = body_start;
            }
            _ => {
                // Unknown / not-of-interest top-level child. Skip.
            }
        }

        // Advance past this element. If the size says it extends past
        // the buffer, we've run out of probe — bail rather than loop.
        if (off + total) > limit + 1 && cid != id::CLUSTER {
            // Cues's nominal end may legitimately equal `limit + 1`
            // when the element fills the probe buffer exactly; that's
            // fine because we already recorded the range above and
            // we'll break out below.
            break;
        }
        off += total;
    }

    let cues = cues?;
    let cluster_start = first_cluster_start?;
    if cues.end >= cluster_start {
        // Cues must precede the first Cluster for the
        // init-then-Cues-then-Clusters layout we synthesize against.
        // Files that put Cues at the tail (after Clusters) need a
        // separate code path; treat them as "no probe data" so the
        // synthesizer falls back to plain BaseURL.
        return None;
    }

    Some(BoxRanges {
        init: ByteRange {
            start: 0,
            end: cues.start.checked_sub(1)?,
        },
        index: cues,
    })
}

/// Fetch the first [`PROBE_BYTES`] bytes of `url` and parse the EBML
/// element list. See [`parse_box_ranges`] for the success/failure
/// criteria.
pub async fn probe(client: &reqwest::Client, url: &str) -> Option<BoxRanges> {
    let res = client
        .get(url)
        .header(reqwest::header::RANGE, format!("bytes=0-{PROBE_BYTES}"))
        .send()
        .await
        .ok()?;
    if !res.status().is_success() {
        warn!(status = %res.status(), "webm probe non-2xx");
        return None;
    }
    let bytes = res.bytes().await.ok()?;
    parse_box_ranges(&bytes)
}

/// Spawn a background tokio task that probes each format
/// sequentially and writes results to the shared `format_box_ranges`
/// table.
///
/// Mirrors [`crate::services::mp4::spawn_background_probes`] but
/// dispatches to the webm probe instead of the mp4 box scanner.
/// Storage and the in-flight dedupe set are deliberately shared with
/// the mp4 module: a single video typically has both webm and mp4
/// formats, and we want the per-video probe lock to cover the union
/// rather than letting webm probes race mp4 probes for the same
/// video.
pub fn spawn_background_probes(pool: SqlitePool, video_id: String, formats: Vec<(String, String)>) {
    if formats.is_empty() {
        return;
    }
    if !crate::services::mp4::register_probe_in_flight(&video_id) {
        // Another worker (mp4 or webm) is already probing this video.
        return;
    }

    let interval_ms = std::env::var("HOMETUBE_PROBE_INTERVAL_MS")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or(crate::services::mp4::DEFAULT_PROBE_INTERVAL_MS);
    let interval = std::time::Duration::from_millis(interval_ms);

    tokio::spawn(async move {
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(10))
            .build()
            .unwrap_or_else(|_| reqwest::Client::new());

        for (format_id, url) in formats {
            if crate::services::mp4::lookup(&pool, &video_id, &format_id)
                .await
                .is_some()
            {
                tokio::time::sleep(interval).await;
                continue;
            }

            if let Some(ranges) = probe(&client, &url).await {
                crate::services::mp4::store(&pool, &video_id, &format_id, ranges).await;
            }

            tokio::time::sleep(interval).await;
        }

        crate::services::mp4::release_probe_in_flight(&video_id);
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Encode a class-A EBML ID (4 bytes, length marker `0x10` in the
    /// top nibble of byte 0) into its on-wire bytes.
    fn class_a_id(id: u32) -> [u8; 4] {
        id.to_be_bytes()
    }

    /// Encode a length on the wire as an 8-byte VINT (`0x01` prefix +
    /// 7 size bytes). The fixed-width encoding keeps the test fixtures
    /// trivial to read; real files use the most compact form.
    fn vint_size_8(n: u64) -> [u8; 8] {
        // Marker bit: 0x01 in byte 0, followed by 7 value bytes
        // (length nybble = 8).
        let mut out = [0u8; 8];
        out[0] = 0x01;
        out[1..].copy_from_slice(&n.to_be_bytes()[1..]);
        out
    }

    /// Build a minimal webm header containing an EBML header, a
    /// Segment, then in order: SeekHead, Info, Tracks, Cues, Cluster.
    /// Element bodies are zero-filled — only the headers and ordering
    /// matter to the parser.
    fn build_webm(
        seek_head_size: usize,
        info_size: usize,
        tracks_size: usize,
        cues_size: usize,
        cluster_size: usize,
    ) -> Vec<u8> {
        let mut out = Vec::new();

        // EBML header: ID + 1-byte size (0x80 = empty), zero body.
        out.extend_from_slice(&class_a_id(0x1A45DFA3));
        out.push(0x80);

        // Segment: ID + 8-byte size (sum of all child element bytes).
        let inner_total = 12
            + seek_head_size
            + 12
            + info_size
            + 12
            + tracks_size
            + 12
            + cues_size
            + 12
            + cluster_size;
        out.extend_from_slice(&class_a_id(id::SEGMENT));
        out.extend_from_slice(&vint_size_8(inner_total as u64));

        // Helper to push a class-A element (ID + 8-byte size + body).
        let mut push_elem = |id_val: u32, size: usize| {
            out.extend_from_slice(&class_a_id(id_val));
            out.extend_from_slice(&vint_size_8(size as u64));
            out.resize(out.len() + size, 0);
        };
        push_elem(id::SEEK_HEAD, seek_head_size);
        push_elem(id::INFO, info_size);
        push_elem(id::TRACKS, tracks_size);
        push_elem(id::CUES, cues_size);
        push_elem(id::CLUSTER, cluster_size);

        out
    }

    #[test]
    fn parses_init_and_cues_ranges() {
        // EBML header: 5 bytes (4 ID + 1 size). Segment header:
        // 12 bytes (4 ID + 8 size). SeekHead/Info/Tracks/Cues/Cluster
        // all use the 12-byte header. SeekHead body=8, Info=10,
        // Tracks=20, Cues=64, Cluster=128.
        let buf = build_webm(8, 10, 20, 64, 128);
        let ranges = parse_box_ranges(&buf).expect("both ranges parse");

        // Layout (offsets):
        //   EBML header: 0..4 (5 bytes)
        //   Segment hdr: 5..16 (12 bytes), body @ 17
        //   SeekHead:    17..36 (12 + 8 = 20 bytes)
        //   Info:        37..58 (12 + 10 = 22 bytes)
        //   Tracks:      59..90 (12 + 20 = 32 bytes)
        //   Cues:        91..166 (12 + 64 = 76 bytes)
        //   Cluster:     167..(167+139)
        let cues_start = 5 + 12 + (12 + 8) + (12 + 10) + (12 + 20);
        let cues_end = cues_start + (12 + 64) - 1;
        assert_eq!(ranges.init.start, 0);
        assert_eq!(ranges.init.end, (cues_start - 1) as u64);
        assert_eq!(ranges.index.start, cues_start as u64);
        assert_eq!(ranges.index.end, cues_end as u64);
    }

    #[test]
    fn returns_none_when_cluster_precedes_cues() {
        // Some webms place Cues at the tail (after Clusters). We
        // can't synthesize a SegmentBase against that layout from a
        // head-only probe, so return None and let the synthesizer
        // fall back to plain BaseURL.
        // Build manually with the wrong order: Cluster before Cues.
        let mut out = Vec::new();
        out.extend_from_slice(&class_a_id(0x1A45DFA3));
        out.push(0x80);

        let inner_total = 12 + 8 + 12 + 32 + 12 + 64;
        out.extend_from_slice(&class_a_id(id::SEGMENT));
        out.extend_from_slice(&vint_size_8(inner_total as u64));

        // SeekHead, then Cluster, then Cues.
        out.extend_from_slice(&class_a_id(id::SEEK_HEAD));
        out.extend_from_slice(&vint_size_8(8));
        out.resize(out.len() + 8, 0);

        out.extend_from_slice(&class_a_id(id::CLUSTER));
        out.extend_from_slice(&vint_size_8(32));
        out.resize(out.len() + 32, 0);

        out.extend_from_slice(&class_a_id(id::CUES));
        out.extend_from_slice(&vint_size_8(64));
        out.resize(out.len() + 64, 0);

        // Parser sees the Cluster first → bails because we require
        // Cues-then-Cluster ordering.
        assert_eq!(parse_box_ranges(&out), None);
    }

    #[test]
    fn returns_none_when_cues_is_missing() {
        // SeekHead → Info → Tracks → Cluster (no Cues in the probe).
        let mut out = Vec::new();
        out.extend_from_slice(&class_a_id(0x1A45DFA3));
        out.push(0x80);

        let inner_total = 12 + 8 + 12 + 10 + 12 + 20 + 12 + 64;
        out.extend_from_slice(&class_a_id(id::SEGMENT));
        out.extend_from_slice(&vint_size_8(inner_total as u64));

        let mut push_elem = |id_val: u32, size: usize| {
            out.extend_from_slice(&class_a_id(id_val));
            out.extend_from_slice(&vint_size_8(size as u64));
            out.resize(out.len() + size, 0);
        };
        push_elem(id::SEEK_HEAD, 8);
        push_elem(id::INFO, 10);
        push_elem(id::TRACKS, 20);
        push_elem(id::CLUSTER, 64);

        assert_eq!(parse_box_ranges(&out), None);
    }

    #[test]
    fn returns_none_when_buffer_does_not_start_with_ebml_header() {
        let buf = vec![0xDE, 0xAD, 0xBE, 0xEF, 0x00, 0x00, 0x00, 0x00];
        assert_eq!(parse_box_ranges(&buf), None);
    }

    #[test]
    fn returns_none_for_truncated_input() {
        assert_eq!(parse_box_ranges(b""), None);
        assert_eq!(parse_box_ranges(b"\x1a\x45"), None);
        // EBML header but truncated mid-Segment.
        let mut buf = Vec::new();
        buf.extend_from_slice(&class_a_id(0x1A45DFA3));
        buf.push(0x80);
        buf.extend_from_slice(&class_a_id(id::SEGMENT));
        buf.push(0x01); // first byte of an 8-byte size — truncated
        assert_eq!(parse_box_ranges(&buf), None);
    }

    /// VINT decoding round-trips for short forms (1 and 2 bytes).
    /// These are the encodings real files use.
    #[test]
    fn vint_short_forms_decode() {
        // 1-byte size: 0x80..0xFF → 0..127 (after masking)
        let buf = [0x85u8]; // 0x80 | 5
        let (v, n) = read_size(&buf, 0).unwrap();
        assert_eq!((v, n), (5, 1));
        // 2-byte size: 0x40_00 → 0; 0x40_64 → 100
        let buf = [0x40, 0x64];
        let (v, n) = read_size(&buf, 0).unwrap();
        assert_eq!((v, n), (100, 2));
    }

    /// Class-A IDs decode preserving the length-marker bit so they
    /// match the canonical constants.
    #[test]
    fn class_a_ids_decode_unmasked() {
        let buf = class_a_id(id::CUES);
        let (id_val, n) = read_id(&buf, 0).unwrap();
        assert_eq!((id_val, n), (id::CUES, 4));

        let buf = class_a_id(id::CLUSTER);
        let (id_val, n) = read_id(&buf, 0).unwrap();
        assert_eq!((id_val, n), (id::CLUSTER, 4));
    }
}
