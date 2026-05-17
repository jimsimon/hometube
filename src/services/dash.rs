//! DASH manifest synthesis and proxy URL signing.
//!
//! HomeTube synthesizes DASH manifests from yt-dlp's per-format metadata,
//! routing each `<Representation>` through signed proxy URLs:
//!
//! ```text
//! <BaseURL>/api/proxy/format?video_id=X&format=248&sig=<hmac></BaseURL>
//! ```
//!
//! The HMAC over the canonical query string prevents abuse — a client
//! can't rewrite arbitrary URLs through our proxy because they cannot
//! produce a valid signature without the server-side secret.
//!
//! The player (shaka-player) drives playback via byte-range requests
//! against the format proxy URL.

use base64::Engine;
use hmac::{Hmac, KeyInit, Mac};
use rand::Rng;
use sha2::Sha256;
use sqlx::SqlitePool;
use tracing::warn;

use crate::error::AppResult;
use crate::services::setup::{get_config_value, set_config_value};
use crate::services::youtube;
use crate::services::ytdlp::Format;

/// `app_config` key for the proxy HMAC secret.
pub const KEY_PROXY_HMAC_SECRET: &str = "proxy_hmac_secret";

type HmacSha256 = Hmac<Sha256>;

/// Read or generate the 32-byte proxy HMAC secret. The secret is stored
/// base64-encoded in `app_config` and survives restarts.
pub async fn ensure_proxy_secret(pool: &SqlitePool) -> AppResult<Vec<u8>> {
    let engine = base64::engine::general_purpose::STANDARD;
    if let Some(stored) = get_config_value(pool, KEY_PROXY_HMAC_SECRET).await? {
        if let Ok(bytes) = engine.decode(stored.as_bytes()) {
            if bytes.len() >= 32 {
                return Ok(bytes);
            }
        }
        warn!("existing proxy_hmac_secret invalid; regenerating");
    }
    let mut bytes = [0u8; 32];
    rand::rng().fill_bytes(&mut bytes);
    let encoded = engine.encode(bytes);
    set_config_value(pool, KEY_PROXY_HMAC_SECRET, &encoded).await?;
    Ok(bytes.to_vec())
}

/// Compute the canonical-query HMAC for the given proxy parameters.
///
/// The signature is over a canonical query string (sorted keys,
/// percent-encoded values), which means the verification side can
/// re-derive the exact same bytes without depending on the order of
/// received params.
pub fn sign_query(secret: &[u8], params: &[(&str, String)]) -> String {
    // Build a canonical string of sorted "k=v" pairs. We use the
    // youtube module's encoder to keep the rules in one place.
    let mut pairs: Vec<(&str, &str)> = params.iter().map(|(k, v)| (*k, v.as_str())).collect();
    pairs.sort();
    let canonical = pairs
        .iter()
        .map(|(k, v)| {
            format!(
                "{}={}",
                youtube::percent_encode(k),
                youtube::percent_encode(v)
            )
        })
        .collect::<Vec<_>>()
        .join("&");

    let mut mac = HmacSha256::new_from_slice(secret).expect("HMAC accepts any key length");
    mac.update(canonical.as_bytes());
    hex_encode(&mac.finalize().into_bytes())
}

/// Tiny lowercase hex encoder — avoids pulling in the `hex` crate just
/// for one helper.
fn hex_encode(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push(HEX[(*b >> 4) as usize] as char);
        out.push(HEX[(*b & 0xf) as usize] as char);
    }
    out
}

/// Verify a previously-signed query. Returns `true` iff the signature
/// matches.
pub fn verify_query(secret: &[u8], params: &[(&str, String)], signature: &str) -> bool {
    let expected = sign_query(secret, params);
    constant_time_eq(expected.as_bytes(), signature.as_bytes())
}

/// Constant-time string compare to thwart timing attacks.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff: u8 = 0;
    for i in 0..a.len() {
        diff |= a[i] ^ b[i];
    }
    diff == 0
}

/// Build a signed proxy URL for streaming an entire format file via
/// byte-range requests. Used by the synthetic DASH manifest where each
/// `<Representation>` points at a `<BaseURL>` through our proxy.
pub fn build_format_proxy_url(secret: &[u8], video_id: &str, format_id: &str) -> String {
    let params: Vec<(&str, String)> = vec![
        ("video_id", video_id.to_string()),
        ("format", format_id.to_string()),
    ];
    let sig = sign_query(secret, &params);
    format!(
        "/api/proxy/format?video_id={}&format={}&sig={}",
        youtube::percent_encode(video_id),
        youtube::percent_encode(format_id),
        sig
    )
}

/// Synthesize a minimal DASH MPD from yt-dlp's per-format metadata.
///
/// This is used when yt-dlp doesn't expose an upstream DASH manifest
/// (common on videos that only offer HLS or direct-download formats).
/// The resulting MPD groups video-only and audio-only formats into
/// separate `<AdaptationSet>`s. Each `<Representation>` renders as a
/// `<BaseURL>` pointing at `/api/proxy/format`, with one of two
/// shapes depending on what we know about the upstream mp4:
///
/// 1. **`<SegmentBase indexRange>` + `<Initialization range>`** — when
///    `box_ranges` contains an entry for the format. the player fetches
///    just the `moov`+`sidx` byte range, parses the sidx to learn
///    segment timing, then issues per-segment range requests against
///    the BaseURL. Each segment is independently cacheable in
///    `/api/proxy/format`. This is the preferred shape for adaptive
///    bitrate switching and per-segment caching.
/// 2. **Plain `<BaseURL>`** — when the format hasn't been probed yet
///    or the probe failed (e.g. raw audio with no sidx). the player
///    issues whole-file byte-range requests; switching qualities
///    abandons in-flight buffer.
///
/// `box_ranges` maps `format_id → BoxRanges`. Callers are expected to
/// pre-populate this from [`crate::services::segment_ranges`] which
/// handles both the cache lookup and the parallel-probe fallback.
/// Passing an empty map is fine and produces case-2 manifests for
/// every format.
///
/// Returns `None` if no usable formats are available (degenerate case
/// — shouldn't happen for normal YouTube videos).
pub fn synthesize_manifest(
    secret: &[u8],
    video_id: &str,
    formats: &[Format],
    duration: Option<f64>,
    box_ranges: &std::collections::HashMap<String, crate::services::segment_ranges::BoxRanges>,
) -> Option<String> {
    // Collect formats whose URLs we can actually use. We accept both
    // `https` (whole-file with byte-range) and `http_dash_segments`
    // (yt-dlp's `-dashy` formats, which also have an un-ranged base
    // URL in `format.url`). Both protocols play through the same
    // BaseURL+Range pipeline; the protocol distinction matters only
    // for dedup.
    //
    // We deliberately surface only `vp9` video and `opus` audio so
    // every Representation lives in a single webm container. That
    // gives us:
    //
    // - One video AdaptationSet (mimeType="video/webm") covering the
    //   full resolution range YouTube provides — vp9 reaches 4K
    //   whereas avc1 caps at 1080p.
    // - One audio AdaptationSet per language (mimeType="audio/webm")
    //   with opus, which matches the video container.
    // - No AdaptationSet split, no per-container codec mismatch
    //   (which previously made the player parse opus/webm as if it
    //   were mp4 and ask for byte 440-million in an 8 MB file).
    //
    // Browser support: vp9 and opus are universal in Chrome/Firefox/
    // Edge and supported in Safari 14+ (2020). For HomeTube on
    // modern hardware this is fine.
    //
    // Storyboard formats (`sb*`) are excluded — they're image sprite
    // sheets, not playable media.
    let is_usable = |f: &&Format| -> bool {
        if f.format_id.starts_with("sb") {
            return false;
        }
        if !matches!(f.protocol.as_deref(), Some("https" | "http_dash_segments")) || f.url.is_none()
        {
            return false;
        }
        // DRC (Dynamic Range Compression) variants share an itag with
        // the standard variant but are different files with different
        // Cues byte offsets. The innertube `/player` API only reports
        // ranges for the non-DRC version, so applying those ranges to
        // a DRC variant makes shaka read garbage bytes and fail with
        // WEBM_CUES_ELEMENT_MISSING. Exclude them entirely — DRC is
        // redundant for our use-case (kids watching on tablets/phones).
        let is_drc = f.format_id.contains("-drc-")
            || f.format_id.ends_with("-drc")
            || f.format_note
                .as_deref()
                .map(|s| s.to_ascii_lowercase().contains("drc"))
                .unwrap_or(false);
        if is_drc {
            return false;
        }

        let vcodec = f.vcodec.as_deref().unwrap_or("none");
        let acodec = f.acodec.as_deref().unwrap_or("none");
        let is_video_only = vcodec != "none" && acodec == "none";
        let is_audio_only = acodec != "none" && vcodec == "none";
        if is_video_only {
            // vp9 is sometimes also reported as `vp09.*` for full
            // codec strings. Accept both spellings.
            vcodec.starts_with("vp9") || vcodec.starts_with("vp09")
        } else if is_audio_only {
            acodec.starts_with("opus")
        } else {
            // Drop muxed (both codecs) and storyboard-like garbage.
            false
        }
    };

    let usable: Vec<&Format> = formats.iter().filter(is_usable).collect();

    // Deduplicate: yt-dlp's `formats=duplicate` extractor flag returns
    // both `https` (whole-file) and `http_dash_segments` (`*-dashy`)
    // variants of the same underlying media. Keeping both clutters
    // the manifest and wastes the player's startup probe budget. We
    // prefer the variant that has BoxRanges available (so it can
    // render as `<SegmentBase>`); when neither has ranges or both do,
    // the first one wins.
    let usable = dedupe_prefer_with_ranges(usable, box_ranges);

    // Initial split into video-only and audio-only Representations.
    // Muxed formats (both vcodec and acodec set) are excluded entirely
    // — they're a separate "progressive" format from yt-dlp that
    // duplicates content already in the adaptive video/audio
    // AdaptationSets. Including them confuses the player.
    let video_candidates: Vec<&Format> = usable
        .iter()
        .copied()
        .filter(|f| {
            f.vcodec.as_deref().unwrap_or("none") != "none"
                && f.acodec.as_deref().unwrap_or("none") == "none"
                && f.height.is_some()
        })
        .collect();
    let audio_candidates: Vec<&Format> = usable
        .iter()
        .copied()
        .filter(|f| {
            f.acodec.as_deref().unwrap_or("none") != "none"
                && f.vcodec.as_deref().unwrap_or("none") == "none"
        })
        
        .collect();

    // Trim the candidate pools so the player doesn't drown in
    // Representations during cold-load discovery (without
    // `<SegmentBase indexRange>`, the player has to probe each
    // Representation empirically; with too many it never converges
    // and playback stalls).
    let video_formats = trim_video_representations(&video_candidates);
    let audio_formats = trim_audio_representations(&audio_candidates);

    if video_formats.is_empty() && audio_formats.is_empty() {
        return None;
    }

    let dur_str = duration
        .map(|d| format!("PT{:.3}S", d))
        .unwrap_or_else(|| "PT0S".to_string());

    let mut mpd = String::with_capacity(4096);
    mpd.push_str("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n");
    mpd.push_str(&format!(
        "<MPD xmlns=\"urn:mpeg:dash:schema:mpd:2011\" type=\"static\" mediaPresentationDuration=\"{}\" minBufferTime=\"PT2S\" profiles=\"urn:mpeg:dash:profile:isoff-on-demand:2011\">\n",
        dur_str
    ));
    mpd.push_str("<Period>\n");

    // Only emit Representations that have SegmentBase data.
    // shaka-player requires indexRange for the isoff-on-demand profile;
    // bare <BaseURL> without SegmentBase triggers error 4003
    // (DASH_NO_SEGMENT_INFO). Formats without ranges are simply
    // omitted — the user gets fewer quality tiers on cold load, but
    // playback works. After background probes or a second extraction
    // fills the cache, the full set appears.
    let video_formats: Vec<&Format> = video_formats
        .into_iter()
        .filter(|f| box_ranges.contains_key(&f.format_id))
        .collect();
    let audio_formats: Vec<&Format> = audio_formats
        .into_iter()
        .filter(|f| box_ranges.contains_key(&f.format_id))
        .collect();

    if video_formats.is_empty() && audio_formats.is_empty() {
        return None;
    }

    if !video_formats.is_empty() {
        mpd.push_str("  <AdaptationSet mimeType=\"video/webm\" contentType=\"video\" segmentAlignment=\"true\" subsegmentStartsWithSAP=\"1\">\n");
        for f in &video_formats {
            let bandwidth = f
                .tbr
                .or(f.vbr)
                .map(|b| (b * 1000.0) as u64)
                .unwrap_or(500_000);
            // Pass yt-dlp's vcodec through verbatim. It's typically
            // the bare string "vp9" but YouTube sometimes returns the
            // fully-qualified `vp09.00.41.08`-style codec ID.
            let codecs = f.vcodec.as_deref().unwrap_or("vp9");
            let attrs = format!(
                "id=\"{}\" bandwidth=\"{}\" width=\"{}\" height=\"{}\" codecs=\"{}\"",
                escape_xml(&f.format_id),
                bandwidth,
                f.width.unwrap_or(0),
                f.height.unwrap_or(0),
                escape_xml(codecs)
            );
            push_representation(&mut mpd, f, &attrs, secret, video_id, box_ranges);
        }
        mpd.push_str("  </AdaptationSet>\n");
    }

    if !audio_formats.is_empty() {
        // Group by language (or "" if absent). BTreeMap gives a stable
        // ordering across calls so the manifest is deterministic.
        let mut lang_groups: std::collections::BTreeMap<String, Vec<&Format>> =
            std::collections::BTreeMap::new();
        for f in &audio_formats {
            let lang = f.language.clone().unwrap_or_default();
            lang_groups.entry(lang).or_default().push(*f);
        }

        let main_lang = pick_main_audio_lang(&audio_formats);

        for (lang, group) in &lang_groups {
            let lang_attr = if lang.is_empty() {
                String::new()
            } else {
                format!(" lang=\"{}\"", escape_xml(lang))
            };
            mpd.push_str(&format!(
                "  <AdaptationSet mimeType=\"audio/webm\" contentType=\"audio\"{}>\n",
                lang_attr
            ));
            // Mark the original-language audio as `Role=main` so
            // the player's `prioritizeRoleMain` selects it. Skip when only
            // one language is present (no ambiguity to resolve).
            if main_lang.as_deref() == Some(lang.as_str()) && lang_groups.len() > 1 {
                mpd.push_str(
                    "    <Role schemeIdUri=\"urn:mpeg:dash:role:2011\" value=\"main\"/>\n",
                );
            }
            for f in group {
                let bandwidth = f
                    .tbr
                    .or(f.abr)
                    .map(|b| (b * 1000.0) as u64)
                    .unwrap_or(128_000);
                let codecs = f.acodec.as_deref().unwrap_or("opus");
                let attrs = format!(
                    "id=\"{}\" bandwidth=\"{}\" codecs=\"{}\"",
                    escape_xml(&f.format_id),
                    bandwidth,
                    escape_xml(codecs)
                );
                push_representation(&mut mpd, f, &attrs, secret, video_id, box_ranges);
            }
            mpd.push_str("  </AdaptationSet>\n");
        }
    }

    mpd.push_str("</Period>\n</MPD>\n");
    Some(mpd)
}

/// Render one `<Representation>` inside the synthesized manifest.
///
/// `attrs` is the pre-rendered attribute string for the
/// `<Representation>` open tag (id, bandwidth, codecs, dimensions).
/// The body is one of two shapes depending on whether we have probed
/// box offsets for this format:
///
/// - **With `BoxRanges`** — `<BaseURL>` + `<SegmentBase indexRange>` +
///   `<Initialization range>`. the player fetches only the moov+sidx
///   prefix to bootstrap, then issues per-segment range requests for
///   playback. Each segment becomes individually cacheable in the
///   format proxy.
/// - **Without `BoxRanges`** — plain `<BaseURL>`. the player performs
///   byte-range fetching against the whole file. Used when probing
///   failed (audio formats without sidx, transient network errors).
fn push_representation(
    mpd: &mut String,
    f: &Format,
    attrs: &str,
    secret: &[u8],
    video_id: &str,
    box_ranges: &std::collections::HashMap<String, crate::services::segment_ranges::BoxRanges>,
) {
    mpd.push_str(&format!("    <Representation {attrs}>\n"));
    let base_url = build_format_proxy_url(secret, video_id, &f.format_id);
    mpd.push_str(&format!(
        "      <BaseURL>{}</BaseURL>\n",
        escape_xml(&base_url)
    ));
    if let Some(ranges) = box_ranges.get(&f.format_id) {
        // SegmentBase path. the player will issue:
        //   1. Range: bytes=<init.start>-<init.end>  → moov (codec init)
        //   2. Range: bytes=<index.start>-<index.end> → sidx (segment table)
        //   3. Range: bytes=...                       → per-segment fetches
        // All three go through /api/proxy/format and our byte-range cache.
        mpd.push_str(&format!(
            "      <SegmentBase indexRange=\"{}\" indexRangeExact=\"true\">\n",
            ranges.index.as_dash()
        ));
        mpd.push_str(&format!(
            "        <Initialization range=\"{}\"/>\n",
            ranges.init.as_dash()
        ));
        mpd.push_str("      </SegmentBase>\n");
    }
    mpd.push_str("    </Representation>\n");
}

/// Drop near-duplicate formats from the usable pool, keeping the
/// variant for which we have probed box ranges (so it can render as
/// `<SegmentBase>`) when the choice is otherwise arbitrary.
///
/// "Same media" is decided by the tuple `(vcodec, acodec, height,
/// width, tbr_kbps_bucket, language)`. tbr is bucketed to integer
/// kbit/s because yt-dlp sometimes reports tiny floating-point
/// differences between the two variants of an otherwise identical
/// stream.
///
/// Order is preserved: the first format we see for each key wins;
/// later variants only override when they have ranges and the
/// existing one doesn't.
fn dedupe_prefer_with_ranges<'a>(
    usable: Vec<&'a Format>,
    box_ranges: &std::collections::HashMap<String, crate::services::segment_ranges::BoxRanges>,
) -> Vec<&'a Format> {
    type Key = (
        Option<String>,
        Option<String>,
        Option<i64>,
        Option<i64>,
        i64,
        Option<String>,
    );
    let mut by_key: std::collections::BTreeMap<Key, &'a Format> = std::collections::BTreeMap::new();
    let mut order: Vec<Key> = Vec::with_capacity(usable.len());

    for f in usable {
        let key: Key = (
            f.vcodec.clone(),
            f.acodec.clone(),
            f.height,
            f.width,
            f.tbr.unwrap_or(0.0) as i64,
            f.language.clone(),
        );
        let has_ranges = box_ranges.contains_key(&f.format_id);
        match by_key.get(&key) {
            None => {
                by_key.insert(key.clone(), f);
                order.push(key);
            }
            Some(existing) => {
                let existing_has_ranges = box_ranges.contains_key(&existing.format_id);
                // Promote the new candidate only when it has ranges
                // and the incumbent doesn't. If both have ranges or
                // neither does, the first one wins (stable order).
                if has_ranges && !existing_has_ranges {
                    by_key.insert(key, f);
                }
            }
        }
    }

    order
        .into_iter()
        .filter_map(|k| by_key.remove(&k))
        .collect()
}

/// Trim the video Representation pool down to one Representation per
/// height. Caller has already filtered to vp9-only, so all candidates
/// share a codec; the only ambiguity is duplicate Representations at
/// the same height (e.g. 30fps vs 60fps, or `https`/`http_dash_segments`
/// variants of the same itag).
///
/// First-seen wins per height — yt-dlp orders formats with the
/// "preferred" variant earlier so that's a reasonable choice.
///
/// We do *not* impose a minimum height. the player inside our trimmed
/// manifest gets at most ~8 video Representations (one per height
/// from 144p to 4K), which is small enough to converge quickly even
/// without `<SegmentBase indexRange>` data.
fn trim_video_representations<'a>(candidates: &[&'a Format]) -> Vec<&'a Format> {
    let mut by_height: std::collections::BTreeMap<i64, &Format> = std::collections::BTreeMap::new();
    for f in candidates {
        let height = match f.height {
            Some(h) => h,
            None => continue,
        };
        // First-seen wins — don't overwrite an existing entry.
        by_height.entry(height).or_insert(*f);
    }
    by_height.into_values().collect()
}

/// Trim the audio Representation pool to one Representation per
/// language. Caller has already filtered to opus-only, so all
/// candidates share a codec; per-language dedupe collapses the
/// quality-tier (`249`/`250`/`251`) and DRC (`*-drc-*`) variants down
/// to a single track.
///
/// One format per language. First-seen wins — this must match the
/// probe order used by `fixup_webm_cues_offsets` (which also takes
/// the first non-DRC non-dub format per itag) so the segment ranges
/// are correct for the format the manifest emits.
fn trim_audio_representations<'a>(candidates: &[&'a Format]) -> Vec<&'a Format> {
    let mut by_lang: std::collections::BTreeMap<String, &Format> =
        std::collections::BTreeMap::new();
    for f in candidates {
        let lang = f.language.clone().unwrap_or_default();
        by_lang.entry(lang).or_insert(*f);
    }
    by_lang.into_values().collect()
}

/// Pick the language tag of the audio track that should be marked as
/// `Role=main` in a synthesized manifest.
///
/// A format whose `format_note` contains the substring `"original"`
/// (case-insensitive) wins, otherwise the format with the highest
/// `language_preference`. Returns `None` when no signal is available.
fn pick_main_audio_lang(audio_formats: &[&Format]) -> Option<String> {
    for f in audio_formats {
        if f.format_note
            .as_deref()
            .map(|s| s.to_ascii_lowercase().contains("original"))
            .unwrap_or(false)
        {
            return f.language.clone();
        }
    }
    let mut best: Option<(&str, i64)> = None;
    for f in audio_formats {
        if let (Some(lang), Some(pref)) = (f.language.as_deref(), f.language_preference) {
            if best.map(|(_, p)| pref > p).unwrap_or(true) {
                best = Some((lang, pref));
            }
        }
    }
    best.map(|(l, _)| l.to_string())
}

/// Minimal XML escaping for attribute and text values in the
/// synthesized MPD. Covers the five characters that have special
/// meaning in attribute and element content per the XML 1.0 spec.
fn escape_xml(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            other => out.push(other),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_signature() {
        let secret = b"test-secret-with-some-bytes-aaaa";
        let params: Vec<(&str, String)> = vec![
            ("video_id", "abc123".into()),
            ("format", "137".into()),
            ("sq", "42".into()),
        ];
        let sig = sign_query(secret, &params);
        assert!(verify_query(secret, &params, &sig));

        let bad: Vec<(&str, String)> = vec![
            ("video_id", "other".into()),
            ("format", "137".into()),
            ("sq", "42".into()),
        ];
        assert!(!verify_query(secret, &bad, &sig));
    }

    /// A signature produced under one secret must not verify under a
    /// different secret — guards against accidental key reuse / leaks
    /// in production rotation.
    #[test]
    fn bad_signature_does_not_verify() {
        let secret_a = b"secret-aaaaaaaaaaaaaaaaaaaaaaaa";
        let secret_b = b"secret-bbbbbbbbbbbbbbbbbbbbbbbb";
        let params: Vec<(&str, String)> = vec![
            ("video_id", "abc123".into()),
            ("format", "137".into()),
            ("sq", "42".into()),
        ];
        let sig = sign_query(secret_a, &params);
        assert!(
            !verify_query(secret_b, &params, &sig),
            "signature must not verify under a different key"
        );

        // A garbage signature also fails (constant-time compare).
        assert!(!verify_query(secret_a, &params, "not-a-real-sig"));
    }

    /// An empty signature must not satisfy `verify_query`. The
    /// constant-time compare returns false on length mismatch, so this
    /// is the API-level surface of the missing-signature case.
    #[test]
    fn missing_signature_does_not_verify() {
        let secret = b"secret-aaaaaaaaaaaaaaaaaaaaaaaa";
        let params: Vec<(&str, String)> = vec![
            ("video_id", "abc123".into()),
            ("format", "137".into()),
            ("sq", "42".into()),
        ];
        assert!(!verify_query(secret, &params, ""));
    }

    /// Parameter ordering must not affect the signature: callers
    /// shouldn't have to sort their inputs.
    #[test]
    fn signature_is_order_independent() {
        let secret = b"secret-aaaaaaaaaaaaaaaaaaaaaaaa";
        let a: Vec<(&str, String)> = vec![
            ("video_id", "v".into()),
            ("format", "137".into()),
            ("sq", "1".into()),
        ];
        let b: Vec<(&str, String)> = vec![
            ("sq", "1".into()),
            ("video_id", "v".into()),
            ("format", "137".into()),
        ];
        assert_eq!(sign_query(secret, &a), sign_query(secret, &b));
    }

    /// Build a `Format` for the synthesizer tests with the minimum
    /// fields needed: `https` protocol, a URL, and codec info.
    fn https_format(
        id: &str,
        height: Option<i64>,
        vcodec: Option<&str>,
        acodec: Option<&str>,
        language: Option<&str>,
    ) -> Format {
        Format {
            format_id: id.into(),
            ext: Some("mp4".into()),
            height,
            width: height.map(|h| h * 16 / 9),
            tbr: Some(1000.0),
            vbr: vcodec.and_then(|c| (c != "none").then_some(800.0)),
            abr: acodec.and_then(|c| (c != "none").then_some(128.0)),
            fps: Some(30.0),
            vcodec: vcodec.map(str::to_owned),
            acodec: acodec.map(str::to_owned),
            filesize: None,
            url: Some(format!(
                "https://example.googlevideo.com/videoplayback?id={id}"
            )),
            manifest_url: None,
            protocol: Some("https".into()),
            language: language.map(str::to_owned),
            language_preference: None,
            format_note: None,
        }
    }

    #[test]
    fn synthesize_manifest_produces_video_and_audio_adaptation_sets() {
        let secret = b"secret-aaaaaaaaaaaaaaaaaaaaaaaa";
        let formats = vec![
            https_format("248", Some(1080), Some("vp9"), Some("none"), None),
            https_format("247", Some(720), Some("vp9"), Some("none"), None),
            https_format("251", None, Some("none"), Some("opus"), Some("en")),
            // Storyboard format must be filtered out.
            Format {
                format_id: "sb0".into(),
                protocol: Some("https".into()),
                url: Some("https://example.com/sb0".into()),
                ..https_format("ignored", None, None, None, None)
            },
        ];
        let br = dummy_ranges(&["248", "247", "251"]);
        let mpd =
            synthesize_manifest(secret, "vid-1", &formats, Some(213.0), &br).expect("synthesize");

        // Well-formed shell.
        assert!(mpd.starts_with("<?xml"));
        assert!(mpd.contains(r#"type="static""#));
        assert!(mpd.contains(r#"mediaPresentationDuration="PT213.000S""#));
        assert!(mpd.contains(r#"profiles="urn:mpeg:dash:profile:isoff-on-demand:2011""#));

        // Both AdaptationSets present with the right contentType.
        assert!(
            mpd.contains(r#"contentType="video""#),
            "video set missing:\n{mpd}"
        );
        assert!(
            mpd.contains(r#"contentType="audio""#),
            "audio set missing:\n{mpd}"
        );

        // Both video Representations present (1080p + 720p) with their
        // proxy BaseURLs.
        assert!(mpd.contains(r#"id="248""#));
        assert!(mpd.contains(r#"id="247""#));
        assert!(mpd.contains("/api/proxy/format?"));
        assert!(mpd.contains("video_id=vid-1"));

        // Audio Representation with language attribute.
        assert!(mpd.contains(r#"id="251""#));
        assert!(mpd.contains(r#"lang="en""#));

        // Storyboard format omitted.
        assert!(!mpd.contains(r#"id="sb0""#), "storyboard leaked:\n{mpd}");

        // BaseURL contents are XML-safe (ampersand-escaped query string).
        assert!(mpd.contains("&amp;format=248"));
    }

    #[test]
    fn synthesize_manifest_returns_none_when_no_https_formats() {
        let secret = b"secret-aaaaaaaaaaaaaaaaaaaaaaaa";
        let formats = vec![Format {
            // m3u8 protocol — not usable for synthesis.
            protocol: Some("m3u8_native".into()),
            ..https_format("96", Some(720), Some("avc1"), Some("aac"), None)
        }];
        let br = std::collections::HashMap::new();
        assert!(synthesize_manifest(secret, "vid", &formats, None, &br).is_none());
    }

    #[test]
    fn synthesize_manifest_marks_original_audio_role_main() {
        let secret = b"secret-aaaaaaaaaaaaaaaaaaaaaaaa";
        let mut en = https_format("251-en", None, Some("none"), Some("opus"), Some("en"));
        en.format_note = Some("original (default), low".into());
        let es = https_format("251-es", None, Some("none"), Some("opus"), Some("es"));
        let br = dummy_ranges(&["251-en", "251-es"]);
        let mpd =
            synthesize_manifest(secret, "vid", &[en, es], Some(60.0), &br).expect("synthesize");

        // Exactly one Role=main, and it's inside the en AdaptationSet.
        assert_eq!(mpd.matches(r#"value="main""#).count(), 1, "{mpd}");
        let en_block = mpd
            .split(r#"lang="en""#)
            .nth(1)
            .expect("en AdaptationSet")
            .split("</AdaptationSet>")
            .next()
            .unwrap();
        assert!(
            en_block.contains(r#"value="main""#),
            "Role=main not in en block: {mpd}"
        );
    }

    #[test]
    fn synthesize_manifest_single_audio_lang_skips_role_main() {
        // Role=main is only meaningful when there's ambiguity to
        // resolve. Single-language audio shouldn't emit it.
        let secret = b"secret-aaaaaaaaaaaaaaaaaaaaaaaa";
        let formats = vec![https_format(
            "251",
            None,
            Some("none"),
            Some("opus"),
            Some("en"),
        )];
        let br = dummy_ranges(&["251"]);
        let mpd = synthesize_manifest(secret, "v", &formats, Some(60.0), &br).expect("synthesize");
        assert!(
            !mpd.contains(r#"value="main""#),
            "unexpected Role=main:\n{mpd}"
        );
    }

    #[test]
    fn synthesize_manifest_proxy_url_signature_round_trips() {
        let secret = b"secret-aaaaaaaaaaaaaaaaaaaaaaaa";
        let url = build_format_proxy_url(secret, "abc", "137");
        // Extract sig and verify under the same params.
        let (_, sig) = url.rsplit_once("sig=").expect("sig in url");
        let params: Vec<(&str, String)> =
            vec![("video_id", "abc".into()), ("format", "137".into())];
        assert!(verify_query(secret, &params, sig));
    }

    /// Build a `BoxRanges` for tests that pre-populate the box-range
    /// map. The exact byte values don't matter for rendering — they
    /// just need to round-trip through the manifest.
    fn ranges(
        init_start: u64,
        init_end: u64,
        idx_start: u64,
        idx_end: u64,
    ) -> super::super::segment_ranges::BoxRanges {
        super::super::segment_ranges::BoxRanges {
            init: super::super::segment_ranges::ByteRange {
                start: init_start,
                end: init_end,
            },
            index: super::super::segment_ranges::ByteRange {
                start: idx_start,
                end: idx_end,
            },
        }
    }

    /// Build a `box_ranges` map with dummy SegmentBase ranges for
    /// every format ID in the slice. Used by tests that don't care
    /// about the actual byte values but need ranges present so the
    /// synthesizer doesn't filter Representations out.
    fn dummy_ranges(
        format_ids: &[&str],
    ) -> std::collections::HashMap<String, super::super::segment_ranges::BoxRanges> {
        format_ids
            .iter()
            .map(|id| (id.to_string(), ranges(0, 219, 220, 4481)))
            .collect()
    }

    /// When `box_ranges` contains an entry for a format, the
    /// synthesizer renders it as `<BaseURL>` plus `<SegmentBase
    /// indexRange>` plus a child `<Initialization range>`. That tells
    /// the player to fetch only the moov+sidx prefix, parse segment
    /// timing, and then issue per-segment range requests against
    /// `/api/proxy/format`.
    #[test]
    fn synthesize_manifest_emits_segment_base_when_ranges_known() {
        let secret = b"secret-aaaaaaaaaaaaaaaaaaaaaaaa";
        let formats = vec![https_format(
            "248",
            Some(1080),
            Some("vp9"),
            Some("none"),
            None,
        )];
        let mut br = std::collections::HashMap::new();
        br.insert("248".to_string(), ranges(32, 511, 512, 4095));

        let mpd =
            synthesize_manifest(secret, "vid-1", &formats, Some(213.0), &br).expect("synthesize");

        assert!(mpd.contains("<BaseURL>"), "BaseURL always emitted:\n{mpd}");
        assert!(
            mpd.contains(r#"<SegmentBase indexRange="512-4095""#),
            "indexRange missing:\n{mpd}"
        );
        assert!(
            mpd.contains(r#"indexRangeExact="true""#),
            "indexRangeExact missing:\n{mpd}"
        );
        assert!(
            mpd.contains(r#"<Initialization range="32-511""#),
            "Initialization range missing:\n{mpd}"
        );
        // Empty SegmentList — the previous incarnation of this code
        // emitted that path; verify we no longer do.
        assert!(
            !mpd.contains("<SegmentList"),
            "SegmentList must not appear in modern synthesizer output:\n{mpd}"
        );
    }

    /// When `box_ranges` is missing for all formats, the synthesizer
    /// now skips those Representations entirely — resulting in no
    /// usable content, so synthesize_manifest returns `None`.
    #[test]
    fn synthesize_manifest_falls_back_to_base_url_when_ranges_unknown() {
        let secret = b"secret-aaaaaaaaaaaaaaaaaaaaaaaa";
        let formats = vec![https_format(
            "248",
            Some(1080),
            Some("vp9"),
            Some("none"),
            None,
        )];
        let br = std::collections::HashMap::new();

        assert!(
            synthesize_manifest(secret, "vid", &formats, Some(60.0), &br).is_none(),
            "manifest should be None when no formats have ranges"
        );
    }

    /// Mixed format pools: ranges available for one format but not
    /// another — now only the format with ranges survives. The format
    /// without ranges is filtered out by the synthesizer.
    #[test]
    fn synthesize_manifest_mixes_segment_base_and_plain_base_url() {
        let secret = b"secret-aaaaaaaaaaaaaaaaaaaaaaaa";
        let formats = vec![
            https_format("248", Some(1080), Some("vp9"), Some("none"), None),
            https_format("247", Some(720), Some("vp9"), Some("none"), None),
        ];
        let mut br = std::collections::HashMap::new();
        br.insert("248".to_string(), ranges(32, 511, 512, 4095));

        let mpd =
            synthesize_manifest(secret, "vid", &formats, Some(60.0), &br).expect("synthesize");
        assert_eq!(
            mpd.matches("<BaseURL>").count(),
            1,
            "one BaseURL (only 248 has ranges):\n{mpd}"
        );
        assert_eq!(
            mpd.matches("<SegmentBase ").count(),
            1,
            "one SegmentBase (only 248 was probed):\n{mpd}"
        );
    }

    /// Dedupe: when both `https` and `http_dash_segments` variants of
    /// the same media exist, the variant that has BoxRanges wins.
    /// This mirrors yt-dlp's actual output with `formats=duplicate`
    /// where every video has both a `248` and `248-dashy` entry.
    #[test]
    fn synthesize_manifest_dedupes_prefers_variant_with_ranges() {
        let secret = b"secret-aaaaaaaaaaaaaaaaaaaaaaaa";
        let mut dashy = https_format("248-dashy", Some(1080), Some("vp9"), Some("none"), None);
        dashy.protocol = Some("http_dash_segments".into());
        let formats = vec![
            // Plain https variant first, with ranges.
            https_format("248", Some(1080), Some("vp9"), Some("none"), None),
            dashy,
        ];
        let mut br = std::collections::HashMap::new();
        br.insert("248".to_string(), ranges(0, 511, 512, 4095));

        let mpd =
            synthesize_manifest(secret, "vid", &formats, Some(60.0), &br).expect("synthesize");
        assert!(
            mpd.contains(r#"id="248""#),
            "ranges-bearing 248 should win:\n{mpd}"
        );
        assert!(
            !mpd.contains(r#"id="248-dashy""#),
            "duplicate dashy variant should be dropped:\n{mpd}"
        );
    }

    /// The synthesizer is vp9-only: avc1 and av01 candidates are
    /// dropped at the `is_usable` filter. When duplicate vp9 variants
    /// exist at the same height (e.g. `https` vs `http_dash_segments`
    /// from `formats=duplicate`), the per-height trim collapses them
    /// to a single Representation — first-seen wins. This keeps
    /// the player's discovery overhead bounded on cold load (without
    /// `<SegmentBase indexRange>`, the player probes each Representation
    /// empirically; too many never converges).
    #[test]
    fn synthesize_manifest_picks_one_codec_per_height() {
        let secret = b"secret-aaaaaaaaaaaaaaaaaaaaaaaa";
        let mut dashy_1080 = https_format("248-dashy", Some(1080), Some("vp9"), Some("none"), None);
        dashy_1080.protocol = Some("http_dash_segments".into());
        let mut dashy_720 = https_format("247-dashy", Some(720), Some("vp9"), Some("none"), None);
        dashy_720.protocol = Some("http_dash_segments".into());
        let formats = vec![
            // 1080p: vp9 wins, avc1/av01 are filtered out by codec
            // gate; duplicate vp9 variant is dropped by per-height trim.
            https_format("248", Some(1080), Some("vp9"), Some("none"), None),
            https_format("137", Some(1080), Some("avc1.640028"), Some("none"), None),
            https_format("399", Some(1080), Some("av01.0.08M.08"), Some("none"), None),
            dashy_1080,
            // 720p: same pattern.
            https_format("247", Some(720), Some("vp9"), Some("none"), None),
            https_format("136", Some(720), Some("avc1.4d401f"), Some("none"), None),
            https_format("398", Some(720), Some("av01.0.05M.08"), Some("none"), None),
            dashy_720,
        ];
        let br = dummy_ranges(&["248", "247", "248-dashy", "247-dashy"]);
        let mpd =
            synthesize_manifest(secret, "vid", &formats, Some(60.0), &br).expect("synthesize");

        // One Representation per height = 2 total.
        let rep_count = mpd.matches("<Representation ").count();
        assert_eq!(rep_count, 2, "expected 2 video Representations:\n{mpd}");
        // First-seen vp9 wins per height.
        assert!(mpd.contains(r#"id="248""#), "1080p vp9 should win:\n{mpd}");
        assert!(mpd.contains(r#"id="247""#), "720p vp9 should win:\n{mpd}");
        // Non-vp9 codecs filtered out.
        assert!(!mpd.contains(r#"id="137""#), "1080p avc1 dropped:\n{mpd}");
        assert!(!mpd.contains(r#"id="399""#), "1080p av01 dropped:\n{mpd}");
        assert!(!mpd.contains(r#"id="136""#), "720p avc1 dropped:\n{mpd}");
        assert!(!mpd.contains(r#"id="398""#), "720p av01 dropped:\n{mpd}");
        // Duplicate vp9 variants collapsed.
        assert!(
            !mpd.contains(r#"id="248-dashy""#),
            "1080p dashy variant dropped:\n{mpd}"
        );
        assert!(
            !mpd.contains(r#"id="247-dashy""#),
            "720p dashy variant dropped:\n{mpd}"
        );
    }

    /// All resolutions (144p through 4K) are kept. HomeTube's intended
    /// use-case spans home wifi (where 4K vp9 is fine) and tethered
    /// hotspots in cars (where 144p is a feature, not a bug — it
    /// keeps playback going on a thin pipe). The synthesizer
    /// deliberately imposes no minimum height; ABR tier selection is
    /// the player's job at runtime.
    #[test]
    fn synthesize_manifest_keeps_all_resolutions() {
        let secret = b"secret-aaaaaaaaaaaaaaaaaaaaaaaa";
        let formats = vec![
            https_format("278", Some(144), Some("vp9"), Some("none"), None),
            https_format("242", Some(240), Some("vp9"), Some("none"), None),
            https_format("243", Some(360), Some("vp9"), Some("none"), None),
            https_format("247", Some(720), Some("vp9"), Some("none"), None),
            https_format("248", Some(1080), Some("vp9"), Some("none"), None),
            https_format("271", Some(1440), Some("vp9"), Some("none"), None),
            https_format("313", Some(2160), Some("vp9"), Some("none"), None),
        ];
        let br = dummy_ranges(&["278", "242", "243", "247", "248", "271", "313"]);
        let mpd =
            synthesize_manifest(secret, "vid", &formats, Some(60.0), &br).expect("synthesize");
        assert!(mpd.contains(r#"id="278""#), "144p must survive:\n{mpd}");
        assert!(mpd.contains(r#"id="242""#), "240p must survive:\n{mpd}");
        assert!(mpd.contains(r#"id="243""#), "360p must survive:\n{mpd}");
        assert!(mpd.contains(r#"id="247""#), "720p must survive:\n{mpd}");
        assert!(mpd.contains(r#"id="248""#), "1080p must survive:\n{mpd}");
        assert!(mpd.contains(r#"id="271""#), "1440p must survive:\n{mpd}");
        assert!(mpd.contains(r#"id="313""#), "2160p must survive:\n{mpd}");
        // One Representation per height — no duplicates.
        assert_eq!(
            mpd.matches("<Representation ").count(),
            7,
            "one Representation per height:\n{mpd}"
        );
    }

    /// Muxed formats (vcodec AND acodec set, like itag 18) duplicate
    /// content already in the adaptive video/audio AdaptationSets and
    /// confuse the player's track selection. Drop them entirely.
    #[test]
    fn synthesize_manifest_drops_muxed_formats() {
        let secret = b"secret-aaaaaaaaaaaaaaaaaaaaaaaa";
        // Hypothetical muxed vp9+opus webm. yt-dlp's classic muxed
        // itag is 18 (avc1+mp4a) which the codec-gate filter would
        // reject anyway; using vp9+opus exercises the muxed-detection
        // logic specifically rather than relying on codec filtering.
        let mut muxed = https_format("18", Some(360), Some("vp9"), Some("opus"), None);
        muxed.acodec = Some("opus".into());
        let formats = vec![
            muxed,
            https_format("247", Some(720), Some("vp9"), Some("none"), None),
            https_format("251", None, Some("none"), Some("opus"), Some("en")),
        ];
        let br = dummy_ranges(&["247", "251"]);
        let mpd =
            synthesize_manifest(secret, "vid", &formats, Some(60.0), &br).expect("synthesize");
        assert!(!mpd.contains(r#"id="18""#), "muxed format dropped:\n{mpd}");
        assert!(mpd.contains(r#"id="247""#), "video-only kept:\n{mpd}");
        assert!(mpd.contains(r#"id="251""#), "audio-only kept:\n{mpd}");
    }

    /// Audio is opus-only: mp4a/AAC candidates are filtered out at the
    /// `is_usable` codec gate (so the audio AdaptationSet stays in
    /// the same webm container as vp9 video). Per-language trim then
    /// collapses opus quality tiers (`249`/`250`/`251`) and the `*-drc-*`
    /// variants down to a single Representation per language —
    /// first-seen wins.
    #[test]
    fn synthesize_manifest_picks_one_audio_codec_per_language() {
        let secret = b"secret-aaaaaaaaaaaaaaaaaaaaaaaa";
        let mut drc = https_format("251-drc", None, Some("none"), Some("opus"), Some("en"));
        drc.format_note = Some("DRC, low".into());
        let formats = vec![
            // Four English candidates: first opus wins, mp4a is
            // filtered out by the codec gate, lower-tier opus and DRC
            // are collapsed by per-language trim.
            https_format("251", None, Some("none"), Some("opus"), Some("en")),
            https_format("250", None, Some("none"), Some("opus"), Some("en")),
            https_format("140", None, Some("none"), Some("mp4a.40.2"), Some("en")),
            drc,
            // Spanish has a single opus candidate — survives.
            https_format("251-es", None, Some("none"), Some("opus"), Some("es")),
        ];
        let br = dummy_ranges(&["251", "250", "251-drc", "251-es"]);
        let mpd =
            synthesize_manifest(secret, "vid", &formats, Some(60.0), &br).expect("synthesize");
        // First-seen English opus wins.
        assert!(
            mpd.contains(r#"id="251""#),
            "first english opus should win:\n{mpd}"
        );
        // mp4a filtered out by the codec gate.
        assert!(
            !mpd.contains(r#"id="140""#),
            "english mp4a dropped (codec gate):\n{mpd}"
        );
        // Lower-tier opus collapsed by per-language trim.
        assert!(
            !mpd.contains(r#"id="250""#),
            "lower-tier english opus dropped:\n{mpd}"
        );
        assert!(
            !mpd.contains(r#"id="251-drc""#),
            "DRC variant dropped:\n{mpd}"
        );
        assert!(
            mpd.contains(r#"id="251-es""#),
            "spanish opus survives (only candidate):\n{mpd}"
        );
        // One AdaptationSet per language = 2 audio Representations.
        let audio_rep_count = mpd.matches("<Representation ").count();
        assert_eq!(
            audio_rep_count, 2,
            "one Representation per language:\n{mpd}"
        );
    }
}
