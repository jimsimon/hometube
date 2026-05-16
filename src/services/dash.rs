//! DASH manifest rewriting.
//!
//! YouTube delivers adaptive video as DASH (MPEG-DASH) manifests pointing
//! at `*.googlevideo.com`. HomeTube parses the manifest, replaces every
//! segment URL with a signed proxy URL, and serves the rewritten manifest
//! to vidstack:
//!
//! ```text
//! Original: https://rr1---sn-xxx.googlevideo.com/videoplayback?...&sq=42
//! Rewritten: /api/proxy/segment?video_id=X&format=137&sq=42&sig=<hmac>
//! ```
//!
//! The HMAC over the canonical query string prevents abuse — a client
//! can't rewrite arbitrary URLs through our proxy because they cannot
//! produce a valid signature without the server-side secret.

use std::io::Cursor;

use base64::Engine;
use hmac::{Hmac, KeyInit, Mac};
use quick_xml::events::{BytesStart, Event};
use quick_xml::reader::Reader;
use quick_xml::writer::Writer;
use rand::Rng;
use sha2::Sha256;
use sqlx::SqlitePool;
use tracing::warn;

use crate::error::{AppError, AppResult};
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
/// The signature is over the canonical query string built by
/// [`youtube::build_canonical_url`] (sorted keys, percent-encoded
/// values), which means the verification side can re-derive the exact
/// same bytes without depending on the order of received params.
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

/// Rewrite a DASH manifest, replacing every `BaseURL`/`SegmentURL`
/// `media`/`initialization` reference with a proxy URL signed for
/// `video_id`. Each rewritten URL also carries the format ID so the
/// proxy can map back to a yt-dlp format on the way out.
///
/// The current implementation tags every segment with the *first*
/// format-id in scope. YouTube manifests put the format ID on the
/// `<Representation>` element via `id=`, and we track it as we walk.
///
/// `formats` is the yt-dlp format list; it's used to identify the
/// "original" (i.e. non-dubbed) audio track on multi-audio videos so
/// we can promote its `<AdaptationSet>` to `Role=main` and demote
/// auto-translated dubs to `Role=alternate`. Without this, dash.js's
/// fallback selection picks an arbitrary AdaptationSet — usually
/// whichever one YouTube put first, which is non-deterministic across
/// requests.
pub fn rewrite_manifest(
    secret: &[u8],
    video_id: &str,
    manifest: &str,
    formats: &[Format],
) -> AppResult<String> {
    // Pre-pass: figure out which audio AdaptationSet (by document
    // order) should be marked Role=main. Returns `None` when the
    // manifest only has one audio track or we can't confidently pick.
    let main_audio_index = pick_main_audio_adaptation_set(manifest, formats)?;

    let mut reader = Reader::from_str(manifest);
    reader.config_mut().trim_text(false);
    let mut writer = Writer::new(Cursor::new(Vec::<u8>::new()));

    // Track the current `<Representation id="...">` so segment URLs
    // know which yt-dlp format they belong to.
    let mut current_format: Option<String> = None;

    // Audio-AdaptationSet bookkeeping. `audio_idx` increments on every
    // audio AdaptationSet open. `inside_audio` tracks whether we're
    // currently inside one; `current_is_main` is set when that audio
    // adaptation set is the one we want to promote.
    let mut audio_idx: usize = 0;
    let mut inside_audio = false;
    let mut current_is_main = false;
    // True after we've emitted (or seen and rewritten) a Role element
    // for the current main AdaptationSet. If false at </AdaptationSet>
    // close we synthesise one so dash.js's `prioritizeRoleMain` picks
    // it up.
    let mut emitted_main_role = false;

    loop {
        match reader
            .read_event()
            .map_err(|e| AppError::Other(anyhow::anyhow!("parsing DASH manifest: {e}")))?
        {
            Event::Eof => break,
            Event::Start(e) => {
                let name_owned = e.name().as_ref().to_vec();
                if name_owned == b"Representation" {
                    if let Some(fmt) = attr_value(&e, b"id") {
                        current_format = Some(fmt);
                    }
                }
                if name_owned == b"AdaptationSet" && is_audio_adaptation_set(&e) {
                    inside_audio = true;
                    current_is_main = main_audio_index == Some(audio_idx);
                    emitted_main_role = false;
                    audio_idx += 1;
                }
                if let Some(rewritten) =
                    rewrite_url_element(&e, secret, video_id, current_format.as_deref(), false)?
                {
                    writer.write_event(Event::Start(rewritten)).ok();
                } else {
                    writer.write_event(Event::Start(e.into_owned())).ok();
                }
            }
            Event::Empty(e) => {
                let name_owned = e.name().as_ref().to_vec();
                // Role descriptors inside an audio AdaptationSet need
                // adjustment so the right track gets `value="main"`.
                if inside_audio && name_owned == b"Role" && is_dash_role_scheme(&e) {
                    let new_value = if current_is_main { "main" } else { "alternate" };
                    let role_el = make_role_element(new_value);
                    writer.write_event(Event::Empty(role_el)).ok();
                    if current_is_main {
                        emitted_main_role = true;
                    }
                    continue;
                }
                if let Some(rewritten) =
                    rewrite_url_element(&e, secret, video_id, current_format.as_deref(), true)?
                {
                    writer.write_event(Event::Empty(rewritten)).ok();
                } else {
                    writer.write_event(Event::Empty(e.into_owned())).ok();
                }
            }
            Event::Text(text) => {
                // <BaseURL>https://...</BaseURL> — child text of a tag we
                // already handled in Start. The rewriting of BaseURLs is
                // performed in `rewrite_url_text` if the element is BaseURL.
                writer.write_event(Event::Text(text.into_owned())).ok();
            }
            Event::End(e) => {
                let name_ref = e.name();
                if name_ref.as_ref() == b"Representation" {
                    current_format = None;
                }
                if name_ref.as_ref() == b"AdaptationSet" && inside_audio {
                    // Inject Role=main for the chosen track if the
                    // upstream manifest didn't include a Role element
                    // we could rewrite in place. dash.js wants the Role
                    // element to be a sibling of <Representation>, not
                    // a child, so we emit it just before </AdaptationSet>.
                    if current_is_main && !emitted_main_role {
                        writer
                            .write_event(Event::Empty(make_role_element("main")))
                            .ok();
                    }
                    inside_audio = false;
                    current_is_main = false;
                    emitted_main_role = false;
                }
                writer.write_event(Event::End(e.into_owned())).ok();
            }
            other => {
                writer.write_event(other).ok();
            }
        }
    }

    let bytes = writer.into_inner().into_inner();
    String::from_utf8(bytes)
        .map_err(|e| AppError::Other(anyhow::anyhow!("encoding rewritten DASH: {e}")))
}

/// Return `true` when this `AdaptationSet` start element carries
/// `mimeType="audio/..."` or `contentType="audio"`.
fn is_audio_adaptation_set(el: &BytesStart<'_>) -> bool {
    if let Some(ct) = attr_value(el, b"contentType") {
        if ct.eq_ignore_ascii_case("audio") {
            return true;
        }
    }
    if let Some(mime) = attr_value(el, b"mimeType") {
        return mime.starts_with("audio/");
    }
    false
}

/// Return `true` when this `Role` element uses the standard DASH role
/// scheme. We deliberately rewrite *only* roles in that scheme so
/// custom/vendor roles in other schemes are preserved verbatim.
fn is_dash_role_scheme(el: &BytesStart<'_>) -> bool {
    match attr_value(el, b"schemeIdUri") {
        Some(s) => s == "urn:mpeg:dash:role:2011" || s == "urn:mpeg:dash:role",
        None => false,
    }
}

/// Build a `<Role schemeIdUri="urn:mpeg:dash:role:2011" value="..."/>`
/// element ready to write into the rewriter.
fn make_role_element(value: &str) -> BytesStart<'static> {
    let mut el = BytesStart::new("Role");
    el.push_attribute(("schemeIdUri", "urn:mpeg:dash:role:2011"));
    el.push_attribute(("value", value));
    el
}

/// Walk the manifest in a first pass and decide which audio
/// `AdaptationSet` (by document order, zero-indexed) should be marked
/// as the "main" track.
///
/// Selection rules, in priority order:
///
/// 1. The AdaptationSet contains a `<Representation id="X">` whose
///    yt-dlp format has `format_note` containing the substring
///    `"original"` (case-insensitive).
/// 2. The AdaptationSet contains a representation whose format has
///    the highest `language_preference` value across all audio
///    representations.
///
/// Returns `None` when the manifest has zero or one audio
/// AdaptationSets, or when no representation has any of the signals
/// above. In the `None` case the rewriter leaves Role elements alone,
/// which preserves the existing (incorrect-but-not-worse) behaviour.
fn pick_main_audio_adaptation_set(manifest: &str, formats: &[Format]) -> AppResult<Option<usize>> {
    let mut reader = Reader::from_str(manifest);
    reader.config_mut().trim_text(false);

    // Per-AdaptationSet collected representation IDs.
    let mut groups: Vec<Vec<String>> = Vec::new();
    let mut current: Option<Vec<String>> = None;

    loop {
        match reader
            .read_event()
            .map_err(|e| AppError::Other(anyhow::anyhow!("parsing DASH manifest: {e}")))?
        {
            Event::Eof => break,
            Event::Start(e) | Event::Empty(e) => {
                let name = e.name().as_ref().to_vec();
                if name == b"AdaptationSet" && is_audio_adaptation_set(&e) {
                    current = Some(Vec::new());
                } else if name == b"Representation" {
                    if let Some(group) = current.as_mut() {
                        if let Some(id) = attr_value(&e, b"id") {
                            group.push(id);
                        }
                    }
                }
            }
            Event::End(e) if e.name().as_ref() == b"AdaptationSet" => {
                if let Some(group) = current.take() {
                    groups.push(group);
                }
            }
            _ => {}
        }
    }

    if groups.len() < 2 {
        return Ok(None);
    }

    // Index formats by id for cheap lookup.
    let by_id = |id: &str| -> Option<&Format> { formats.iter().find(|f| f.format_id == id) };

    // Rule 1: any group containing a representation tagged "original".
    for (idx, group) in groups.iter().enumerate() {
        for rep_id in group {
            if let Some(f) = by_id(rep_id) {
                if f.format_note
                    .as_deref()
                    .map(|s| s.to_ascii_lowercase().contains("original"))
                    .unwrap_or(false)
                {
                    return Ok(Some(idx));
                }
            }
        }
    }

    // Rule 2: highest language_preference. The "original" track on
    // multi-audio YouTube videos scores 10; auto-dubs are typically
    // -1 or absent.
    let mut best: Option<(usize, i64)> = None;
    for (idx, group) in groups.iter().enumerate() {
        for rep_id in group {
            if let Some(f) = by_id(rep_id) {
                if let Some(pref) = f.language_preference {
                    if best.map(|(_, p)| pref > p).unwrap_or(true) {
                        best = Some((idx, pref));
                    }
                }
            }
        }
    }
    Ok(best.map(|(idx, _)| idx))
}

/// Build the proxy URL for a segment. The `sq` parameter is the segment
/// sequence number; the rest of the URL is recoverable from the cached
/// metadata + format ID.
pub fn build_segment_proxy_url(secret: &[u8], video_id: &str, format_id: &str, sq: &str) -> String {
    let params: Vec<(&str, String)> = vec![
        ("video_id", video_id.to_string()),
        ("format", format_id.to_string()),
        ("sq", sq.to_string()),
    ];
    let sig = sign_query(secret, &params);
    format!(
        "/api/proxy/segment?video_id={}&format={}&sq={}&sig={}",
        youtube::percent_encode(video_id),
        youtube::percent_encode(format_id),
        youtube::percent_encode(sq),
        sig
    )
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
///    `box_ranges` contains an entry for the format. dash.js fetches
///    just the `moov`+`sidx` byte range, parses the sidx to learn
///    segment timing, then issues per-segment range requests against
///    the BaseURL. Each segment is independently cacheable in
///    `/api/proxy/format`. This is the preferred shape for adaptive
///    bitrate switching and per-segment caching.
/// 2. **Plain `<BaseURL>`** — when the format hasn't been probed yet
///    or the probe failed (e.g. raw audio with no sidx). dash.js
///    issues whole-file byte-range requests; switching qualities
///    abandons in-flight buffer.
///
/// `box_ranges` maps `format_id → BoxRanges`. Callers are expected to
/// pre-populate this from [`crate::services::mp4::resolve_all`] which
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
    box_ranges: &std::collections::HashMap<String, crate::services::mp4::BoxRanges>,
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
    //   (which previously made dash.js parse opus/webm as if it
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
    // the manifest and wastes dash.js's startup probe budget. We
    // prefer the variant that has BoxRanges available (so it can
    // render as `<SegmentBase>`); when neither has ranges or both do,
    // the first one wins.
    let usable = dedupe_prefer_with_ranges(usable, box_ranges);

    // Initial split into video-only and audio-only Representations.
    // Muxed formats (both vcodec and acodec set) are excluded entirely
    // — they're a separate "progressive" format from yt-dlp that
    // duplicates content already in the adaptive video/audio
    // AdaptationSets. Including them confuses dash.js.
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

    // Trim the candidate pools so dash.js doesn't drown in
    // Representations during cold-load discovery (without
    // `<SegmentBase indexRange>`, dash.js has to probe each
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
            // dash.js's `prioritizeRoleMain` selects it. Skip when only
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
///   `<Initialization range>`. dash.js fetches only the moov+sidx
///   prefix to bootstrap, then issues per-segment range requests for
///   playback. Each segment becomes individually cacheable in the
///   format proxy.
/// - **Without `BoxRanges`** — plain `<BaseURL>`. dash.js performs
///   byte-range fetching against the whole file. Used when probing
///   failed (audio formats without sidx, transient network errors).
fn push_representation(
    mpd: &mut String,
    f: &Format,
    attrs: &str,
    secret: &[u8],
    video_id: &str,
    box_ranges: &std::collections::HashMap<String, crate::services::mp4::BoxRanges>,
) {
    mpd.push_str(&format!("    <Representation {attrs}>\n"));
    let base_url = build_format_proxy_url(secret, video_id, &f.format_id);
    mpd.push_str(&format!(
        "      <BaseURL>{}</BaseURL>\n",
        escape_xml(&base_url)
    ));
    if let Some(ranges) = box_ranges.get(&f.format_id) {
        // SegmentBase path. dash.js will issue:
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
    box_ranges: &std::collections::HashMap<String, crate::services::mp4::BoxRanges>,
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
/// We do *not* impose a minimum height. dash.js inside our trimmed
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
/// First-seen wins per language. yt-dlp orders by quality with the
/// preferred variant first.
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
/// Mirrors the heuristics used by [`rewrite_manifest`]: a format whose
/// `format_note` contains the substring `"original"` (case-insensitive)
/// wins, otherwise the format with the highest `language_preference`.
/// Returns `None` when no signal is available.
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

/// If `el` is a `<SegmentURL>` or `<BaseURL>` (and friends), return a
/// rewritten version. Otherwise `Ok(None)`.
fn rewrite_url_element<'a>(
    el: &BytesStart<'a>,
    secret: &[u8],
    video_id: &str,
    format_id: Option<&str>,
    _empty: bool,
) -> AppResult<Option<BytesStart<'static>>> {
    let name = el.name().as_ref().to_vec();
    if name != b"SegmentURL" && name != b"SegmentTemplate" {
        return Ok(None);
    }
    let format = format_id.unwrap_or("");

    let mut new_el = BytesStart::new(String::from_utf8_lossy(&name).into_owned());
    for attr in el.attributes().with_checks(false).flatten() {
        let key = attr.key.as_ref().to_vec();
        let val = String::from_utf8_lossy(&attr.value).to_string();

        let new_val = match key.as_slice() {
            // SegmentURL@media — the actual segment URL.
            b"media" | b"initialization" | b"sourceURL" => {
                let sq = extract_sq(&val);
                build_segment_proxy_url(secret, video_id, format, sq.as_deref().unwrap_or(""))
            }
            _ => val,
        };
        new_el.push_attribute((std::str::from_utf8(&key).unwrap_or(""), new_val.as_str()));
    }
    Ok(Some(new_el))
}

/// Pull the `sq=...` value out of a googlevideo URL. Falls back to the
/// `$Number$` template token in `<SegmentTemplate>` URLs (we keep the
/// token verbatim and let the player fill it in — vidstack doesn't
/// rewrite the template, so passing through is enough to round-trip).
fn extract_sq(url: &str) -> Option<String> {
    if let Some(idx) = url.find("sq=") {
        let rest = &url[idx + 3..];
        let end = rest.find('&').unwrap_or(rest.len());
        return Some(rest[..end].to_string());
    }
    if url.contains("$Number$") {
        return Some("$Number$".to_string());
    }
    None
}

fn attr_value(el: &BytesStart<'_>, name: &[u8]) -> Option<String> {
    for attr in el.attributes().with_checks(false).flatten() {
        if attr.key.as_ref() == name {
            return Some(String::from_utf8_lossy(&attr.value).to_string());
        }
    }
    None
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

    /// `build_segment_proxy_url` produces a URL that contains the
    /// signature it would itself verify against.
    #[test]
    fn build_segment_proxy_url_round_trip() {
        let secret = b"secret-aaaaaaaaaaaaaaaaaaaaaaaa";
        let url = build_segment_proxy_url(secret, "abc", "137", "42");
        // Find sig=...
        let (_, sig) = url.rsplit_once("sig=").expect("sig in url");
        let params: Vec<(&str, String)> = vec![
            ("video_id", "abc".into()),
            ("format", "137".into()),
            ("sq", "42".into()),
        ];
        assert!(verify_query(secret, &params, sig));
    }

    /// `rewrite_manifest` rewrites every `<SegmentURL media=…>` so the
    /// resulting URL is a signed proxy URL. Garbage XML is preserved
    /// verbatim outside the rewritten attributes.
    #[test]
    fn rewrite_manifest_swaps_segment_urls() {
        let secret = b"secret-aaaaaaaaaaaaaaaaaaaaaaaa";
        let manifest = r#"<?xml version="1.0"?>
<MPD>
  <Period>
    <AdaptationSet>
      <Representation id="137">
        <SegmentList>
          <SegmentURL media="https://rr1.googlevideo.com/x?sq=42&amp;y=1"/>
          <SegmentURL media="https://rr1.googlevideo.com/x?sq=43"/>
        </SegmentList>
      </Representation>
    </AdaptationSet>
  </Period>
</MPD>"#;
        let rewritten = rewrite_manifest(secret, "video-1", manifest, &[]).expect("rewrite");
        // No more raw googlevideo URLs.
        assert!(!rewritten.contains("googlevideo.com"));
        // Proxy URLs include the original sq value.
        assert!(rewritten.contains("/api/proxy/segment"));
        assert!(rewritten.contains("sq=42"));
        assert!(rewritten.contains("sq=43"));
        // Format-id is recovered from the parent <Representation>.
        assert!(rewritten.contains("format=137"));
    }

    /// Verify that a manifest without any segment URLs round-trips
    /// without modification.
    #[test]
    fn rewrite_manifest_passthrough() {
        let secret = b"secret-aaaaaaaaaaaaaaaaaaaaaaaa";
        let manifest = "<?xml version=\"1.0\"?><Empty/>";
        let rewritten = rewrite_manifest(secret, "v", manifest, &[]).expect("rewrite");
        // Should not contain any proxy URL since there are no segments.
        assert!(!rewritten.contains("/api/proxy/segment"));
    }

    /// Build a `Format` with the minimum fields needed for the
    /// audio-language tests.
    fn audio_format(id: &str, language: &str, pref: Option<i64>, note: Option<&str>) -> Format {
        Format {
            format_id: id.into(),
            ext: None,
            height: None,
            width: None,
            tbr: None,
            vbr: None,
            abr: None,
            fps: None,
            vcodec: Some("none".into()),
            acodec: Some("mp4a.40.2".into()),
            filesize: None,
            url: None,
            manifest_url: None,
            protocol: None,
            language: Some(language.into()),
            language_preference: pref,
            format_note: note.map(str::to_owned),
        }
    }

    /// Multi-audio manifest where the *second* AdaptationSet contains
    /// the original audio. The rewriter must promote it to Role=main
    /// and demote the (previously-marked-main) first AdaptationSet to
    /// Role=alternate so dash.js's `prioritizeRoleMain` selection
    /// picks the original.
    #[test]
    fn rewrite_manifest_promotes_original_audio_via_format_note() {
        let secret = b"secret-aaaaaaaaaaaaaaaaaaaaaaaa";
        let manifest = r#"<?xml version="1.0"?>
<MPD>
  <Period>
    <AdaptationSet mimeType="audio/mp4" lang="es" id="1">
      <Role schemeIdUri="urn:mpeg:dash:role:2011" value="main"/>
      <Representation id="251-1" />
    </AdaptationSet>
    <AdaptationSet mimeType="audio/mp4" lang="en" id="2">
      <Representation id="251-2" />
    </AdaptationSet>
  </Period>
</MPD>"#;
        let formats = vec![
            audio_format("251-1", "es", Some(-1), Some("Spanish dub, low")),
            audio_format("251-2", "en", Some(10), Some("original (default), low")),
        ];
        let rewritten = rewrite_manifest(secret, "v", manifest, &formats).expect("rewrite");

        // The originally-main Spanish AdaptationSet should now be
        // demoted; the English one should be the only `value="main"`.
        let main_count = rewritten.matches(r#"value="main""#).count();
        assert_eq!(
            main_count, 1,
            "exactly one Role=main expected:\n{rewritten}"
        );
        let alt_count = rewritten.matches(r#"value="alternate""#).count();
        assert_eq!(alt_count, 1, "the dub should be demoted:\n{rewritten}");

        // Specifically: the main role should sit inside the AdaptationSet
        // whose lang="en" attribute appears earlier in the document.
        let en_block = rewritten
            .split(r#"lang="en""#)
            .nth(1)
            .expect("english adaptation set in output");
        let en_block_until_close = en_block.split("</AdaptationSet>").next().unwrap();
        assert!(
            en_block_until_close.contains(r#"value="main""#),
            "english AdaptationSet should carry Role=main:\n{rewritten}"
        );
    }

    /// When yt-dlp doesn't surface a `format_note` we fall back to
    /// `language_preference`. Highest preference wins.
    #[test]
    fn rewrite_manifest_promotes_via_language_preference() {
        let secret = b"secret-aaaaaaaaaaaaaaaaaaaaaaaa";
        let manifest = r#"<?xml version="1.0"?>
<MPD>
  <Period>
    <AdaptationSet mimeType="audio/mp4" lang="es" id="1">
      <Representation id="A" />
    </AdaptationSet>
    <AdaptationSet mimeType="audio/mp4" lang="en" id="2">
      <Representation id="B" />
    </AdaptationSet>
  </Period>
</MPD>"#;
        let formats = vec![
            audio_format("A", "es", Some(-1), None),
            audio_format("B", "en", Some(10), None),
        ];
        let rewritten = rewrite_manifest(secret, "v", manifest, &formats).expect("rewrite");
        // No upstream Role=main existed, so we should have *injected*
        // exactly one Role=main inside the second AdaptationSet.
        let main_count = rewritten.matches(r#"value="main""#).count();
        assert_eq!(main_count, 1, "synthesised Role=main missing:\n{rewritten}");
        let en_block = rewritten
            .split(r#"lang="en""#)
            .nth(1)
            .expect("english adaptation set in output")
            .split("</AdaptationSet>")
            .next()
            .unwrap();
        assert!(
            en_block.contains(r#"value="main""#),
            "english AdaptationSet should carry the synthesised Role=main:\n{rewritten}"
        );
    }

    /// With only one audio AdaptationSet the rewriter must not touch
    /// any Role elements — the manifest is already unambiguous and
    /// inserting a Role could break perfectly fine playback.
    #[test]
    fn rewrite_manifest_single_audio_set_is_left_alone() {
        let secret = b"secret-aaaaaaaaaaaaaaaaaaaaaaaa";
        let manifest = r#"<?xml version="1.0"?>
<MPD>
  <Period>
    <AdaptationSet mimeType="audio/mp4" lang="en">
      <Representation id="A" />
    </AdaptationSet>
  </Period>
</MPD>"#;
        let formats = vec![audio_format("A", "en", Some(10), Some("original"))];
        let rewritten = rewrite_manifest(secret, "v", manifest, &formats).expect("rewrite");
        // No Role elements anywhere — the original had none, so we
        // shouldn't have invented one.
        assert!(
            !rewritten.contains("<Role"),
            "no Role expected:\n{rewritten}"
        );
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
        let br = std::collections::HashMap::new();
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
        let br = std::collections::HashMap::new();
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
        let br = std::collections::HashMap::new();
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
    ) -> super::super::mp4::BoxRanges {
        super::super::mp4::BoxRanges {
            init: super::super::mp4::ByteRange {
                start: init_start,
                end: init_end,
            },
            index: super::super::mp4::ByteRange {
                start: idx_start,
                end: idx_end,
            },
        }
    }

    /// When `box_ranges` contains an entry for a format, the
    /// synthesizer renders it as `<BaseURL>` plus `<SegmentBase
    /// indexRange>` plus a child `<Initialization range>`. That tells
    /// dash.js to fetch only the moov+sidx prefix, parse segment
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

    /// When `box_ranges` is missing for a format, the synthesizer
    /// falls back to plain `<BaseURL>` (no `<SegmentBase>`). dash.js
    /// will then byte-range-fetch the entire file. Used for formats
    /// where the probe failed or hasn't run yet (audio with no sidx,
    /// transient probe errors).
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

        let mpd =
            synthesize_manifest(secret, "vid", &formats, Some(60.0), &br).expect("synthesize");
        assert!(mpd.contains("<BaseURL>"), "BaseURL always emitted:\n{mpd}");
        assert!(
            !mpd.contains("<SegmentBase"),
            "SegmentBase only when ranges are known:\n{mpd}"
        );
        assert!(
            !mpd.contains("<Initialization"),
            "Initialization only when ranges are known:\n{mpd}"
        );
    }

    /// Mixed format pools: ranges available for one format but not
    /// another should produce a manifest with a SegmentBase block on
    /// the probed format and a plain BaseURL on the other. dash.js
    /// handles this fine — each Representation is independent.
    /// Uses two different heights so the per-height trim doesn't drop
    /// either Representation.
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
        assert_eq!(mpd.matches("<BaseURL>").count(), 2, "two BaseURLs:\n{mpd}");
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
    /// dash.js's discovery overhead bounded on cold load (without
    /// `<SegmentBase indexRange>`, dash.js probes each Representation
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
        let br = std::collections::HashMap::new();
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
    /// dash.js's job at runtime.
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
        let br = std::collections::HashMap::new();
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
    /// confuse dash.js's track selection. Drop them entirely.
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
        let br = std::collections::HashMap::new();
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
        let br = std::collections::HashMap::new();
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

    /// Custom (non-`urn:mpeg:dash:role:*`) Role schemes must round-trip
    /// untouched even inside an audio AdaptationSet that we're
    /// rewriting — those describe orthogonal axes (accessibility,
    /// vendor extensions) and rewriting them risks breaking playback.
    #[test]
    fn rewrite_manifest_preserves_non_dash_role_scheme() {
        let secret = b"secret-aaaaaaaaaaaaaaaaaaaaaaaa";
        let manifest = r#"<?xml version="1.0"?>
<MPD>
  <Period>
    <AdaptationSet mimeType="audio/mp4" lang="es" id="1">
      <Role schemeIdUri="urn:vendor:custom" value="main"/>
      <Representation id="A" />
    </AdaptationSet>
    <AdaptationSet mimeType="audio/mp4" lang="en" id="2">
      <Representation id="B" />
    </AdaptationSet>
  </Period>
</MPD>"#;
        let formats = vec![
            audio_format("A", "es", Some(-1), None),
            audio_format("B", "en", Some(10), Some("original")),
        ];
        let rewritten = rewrite_manifest(secret, "v", manifest, &formats).expect("rewrite");
        // The vendor Role should still carry its original scheme.
        assert!(
            rewritten.contains(r#"schemeIdUri="urn:vendor:custom""#),
            "vendor Role scheme stripped:\n{rewritten}"
        );
    }
}
