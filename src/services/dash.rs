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
    // Storyboard formats (`sb*`) are excluded — they're image sprite
    // sheets, not playable media.
    let is_usable = |f: &&Format| -> bool {
        if f.format_id.starts_with("sb") {
            return false;
        }
        matches!(f.protocol.as_deref(), Some("https" | "http_dash_segments")) && f.url.is_some()
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
        mpd.push_str("  <AdaptationSet mimeType=\"video/mp4\" contentType=\"video\" segmentAlignment=\"true\" subsegmentStartsWithSAP=\"1\">\n");
        for f in &video_formats {
            let bandwidth = f
                .tbr
                .or(f.vbr)
                .map(|b| (b * 1000.0) as u64)
                .unwrap_or(500_000);
            let codecs = f.vcodec.as_deref().unwrap_or("avc1.4d401f");
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
                "  <AdaptationSet mimeType=\"audio/mp4\" contentType=\"audio\"{}>\n",
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
                let codecs = f.acodec.as_deref().unwrap_or("mp4a.40.2");
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

/// Trim the video Representation pool down to a small, dash.js-friendly
/// list. Without `<SegmentBase indexRange>` dash.js has to discover
/// each Representation's size empirically by issuing `Range:` requests;
/// surfacing every codec at every resolution causes it to thrash and
/// stall before converging on a playable Rep.
///
/// Strategy:
///
/// 1. Drop heights below 360 — those resolutions are unwatchable on
///    anything bigger than a phone and dash.js sometimes picks them as
///    starting quality before bandwidth measurement settles.
/// 2. Group remaining formats by `height`. For each height bucket pick
///    one Representation, preferring `avc1`/`H.264` for maximum
///    browser compatibility, then `vp9`, then `av01`. Within a tied
///    codec choice the one we saw first wins (yt-dlp's natural
///    ordering puts the "main" variant earlier).
fn trim_video_representations<'a>(candidates: &[&'a Format]) -> Vec<&'a Format> {
    /// Lower codec score = stronger preference.
    fn codec_pref(vcodec: Option<&str>) -> u8 {
        match vcodec.unwrap_or("") {
            c if c.starts_with("avc1") || c.starts_with("h264") => 0,
            c if c.starts_with("vp9") || c.starts_with("vp09") => 1,
            c if c.starts_with("av01") || c.starts_with("av1") => 2,
            _ => 3,
        }
    }

    let mut by_height: std::collections::BTreeMap<i64, &Format> = std::collections::BTreeMap::new();
    for f in candidates {
        let height = match f.height {
            Some(h) if h >= 360 => h,
            _ => continue,
        };
        let pref = codec_pref(f.vcodec.as_deref());
        match by_height.get(&height) {
            None => {
                by_height.insert(height, *f);
            }
            Some(existing) => {
                let existing_pref = codec_pref(existing.vcodec.as_deref());
                if pref < existing_pref {
                    by_height.insert(height, *f);
                }
            }
        }
    }
    by_height.into_values().collect()
}

/// Trim the audio Representation pool. dash.js handles audio
/// AdaptationSets less aggressively than video (smaller files, less
/// adaptive switching), but we still want to limit the count to keep
/// cold-load discovery cheap.
///
/// Strategy:
///
/// 1. Drop "DRC" variants (yt-dlp's `*-drc-dashy` formats) — these
///    are alternate Dynamic Range Compression renditions and dash.js
///    treats them as duplicate audio tracks.
/// 2. Group by language. For each language pick one Representation,
///    preferring `mp4a`/AAC over `opus` for Safari compatibility.
fn trim_audio_representations<'a>(candidates: &[&'a Format]) -> Vec<&'a Format> {
    fn codec_pref(acodec: Option<&str>) -> u8 {
        match acodec.unwrap_or("") {
            c if c.starts_with("mp4a") || c.starts_with("aac") => 0,
            c if c.starts_with("opus") => 1,
            c if c.starts_with("vorbis") => 2,
            _ => 3,
        }
    }

    // BTreeMap so output ordering is stable across calls.
    let mut by_lang: std::collections::BTreeMap<String, &Format> =
        std::collections::BTreeMap::new();
    for f in candidates {
        // Drop DRC variants. yt-dlp tags them in two ways: a
        // `*-drc-*` substring in the format_id, or a "DRC" token
        // somewhere in `format_note`. Catching both is cheap.
        if f.format_id.contains("-drc")
            || f.format_note
                .as_deref()
                .map(|s| s.to_ascii_lowercase().contains("drc"))
                .unwrap_or(false)
        {
            continue;
        }
        let lang = f.language.clone().unwrap_or_default();
        let pref = codec_pref(f.acodec.as_deref());
        match by_lang.get(&lang) {
            None => {
                by_lang.insert(lang, *f);
            }
            Some(existing) => {
                let existing_pref = codec_pref(existing.acodec.as_deref());
                if pref < existing_pref {
                    by_lang.insert(lang, *f);
                }
            }
        }
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
            https_format("137", Some(1080), Some("avc1.640028"), Some("none"), None),
            https_format("136", Some(720), Some("avc1.4d401f"), Some("none"), None),
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
        assert!(mpd.contains(r#"id="137""#));
        assert!(mpd.contains(r#"id="136""#));
        assert!(mpd.contains("/api/proxy/format?"));
        assert!(mpd.contains("video_id=vid-1"));

        // Audio Representation with language attribute.
        assert!(mpd.contains(r#"id="251""#));
        assert!(mpd.contains(r#"lang="en""#));

        // Storyboard format omitted.
        assert!(!mpd.contains(r#"id="sb0""#), "storyboard leaked:\n{mpd}");

        // BaseURL contents are XML-safe (ampersand-escaped query string).
        assert!(mpd.contains("&amp;format=137"));
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
            "137",
            Some(1080),
            Some("avc1.640028"),
            Some("none"),
            None,
        )];
        let mut br = std::collections::HashMap::new();
        br.insert("137".to_string(), ranges(32, 511, 512, 4095));

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
            "137",
            Some(1080),
            Some("avc1.640028"),
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
            https_format("137", Some(1080), Some("avc1.640028"), Some("none"), None),
            https_format("136", Some(720), Some("avc1.4d401f"), Some("none"), None),
        ];
        let mut br = std::collections::HashMap::new();
        br.insert("137".to_string(), ranges(32, 511, 512, 4095));

        let mpd =
            synthesize_manifest(secret, "vid", &formats, Some(60.0), &br).expect("synthesize");
        assert_eq!(mpd.matches("<BaseURL>").count(), 2, "two BaseURLs:\n{mpd}");
        assert_eq!(
            mpd.matches("<SegmentBase ").count(),
            1,
            "one SegmentBase (only 137 was probed):\n{mpd}"
        );
    }

    /// Dedupe: when both `https` and `http_dash_segments` variants of
    /// the same media exist, the variant that has BoxRanges wins.
    /// This mirrors yt-dlp's actual output with `formats=duplicate`
    /// where every video has both a `137` and `137-dashy` entry.
    #[test]
    fn synthesize_manifest_dedupes_prefers_variant_with_ranges() {
        let secret = b"secret-aaaaaaaaaaaaaaaaaaaaaaaa";
        let mut dashy = https_format(
            "137-dashy",
            Some(1080),
            Some("avc1.640028"),
            Some("none"),
            None,
        );
        dashy.protocol = Some("http_dash_segments".into());
        let formats = vec![
            // Plain https variant first, with ranges.
            https_format("137", Some(1080), Some("avc1.640028"), Some("none"), None),
            dashy,
        ];
        let mut br = std::collections::HashMap::new();
        br.insert("137".to_string(), ranges(0, 511, 512, 4095));

        let mpd =
            synthesize_manifest(secret, "vid", &formats, Some(60.0), &br).expect("synthesize");
        assert!(
            mpd.contains(r#"id="137""#),
            "ranges-bearing 137 should win:\n{mpd}"
        );
        assert!(
            !mpd.contains(r#"id="137-dashy""#),
            "duplicate dashy variant should be dropped:\n{mpd}"
        );
    }

    /// Multiple codecs at the same height should collapse to one
    /// Representation per height, with avc1 winning over vp9 and
    /// av01. This keeps dash.js's discovery overhead bounded on cold
    /// load (when no SegmentBase data is available yet).
    #[test]
    fn synthesize_manifest_picks_one_codec_per_height() {
        let secret = b"secret-aaaaaaaaaaaaaaaaaaaaaaaa";
        let formats = vec![
            // Three codecs at 1080p; avc1 should win.
            https_format("248", Some(1080), Some("vp9"), Some("none"), None),
            https_format("137", Some(1080), Some("avc1.640028"), Some("none"), None),
            https_format("399", Some(1080), Some("av01.0.08M.08"), Some("none"), None),
            // Three at 720p; same again.
            https_format("247", Some(720), Some("vp9"), Some("none"), None),
            https_format("136", Some(720), Some("avc1.4d401f"), Some("none"), None),
            https_format("398", Some(720), Some("av01.0.05M.08"), Some("none"), None),
        ];
        let br = std::collections::HashMap::new();
        let mpd =
            synthesize_manifest(secret, "vid", &formats, Some(60.0), &br).expect("synthesize");

        // One Representation per height = 2 total.
        let rep_count = mpd.matches("<Representation ").count();
        assert_eq!(rep_count, 2, "expected 2 video Representations:\n{mpd}");
        assert!(mpd.contains(r#"id="137""#), "1080p avc1 should win:\n{mpd}");
        assert!(mpd.contains(r#"id="136""#), "720p avc1 should win:\n{mpd}");
        // Other codec variants must be dropped.
        assert!(!mpd.contains(r#"id="248""#), "1080p vp9 dropped:\n{mpd}");
        assert!(!mpd.contains(r#"id="399""#), "1080p av01 dropped:\n{mpd}");
        assert!(!mpd.contains(r#"id="247""#), "720p vp9 dropped:\n{mpd}");
        assert!(!mpd.contains(r#"id="398""#), "720p av01 dropped:\n{mpd}");
    }

    /// Heights below 360 are unwatchable on modern devices and confuse
    /// dash.js's startup ABR logic — they get dropped before the
    /// per-height collapse runs.
    #[test]
    fn synthesize_manifest_drops_low_resolutions() {
        let secret = b"secret-aaaaaaaaaaaaaaaaaaaaaaaa";
        let formats = vec![
            https_format("160", Some(144), Some("avc1.4d400c"), Some("none"), None),
            https_format("133", Some(240), Some("avc1.4d4015"), Some("none"), None),
            https_format("134", Some(360), Some("avc1.4d401e"), Some("none"), None),
            https_format("136", Some(720), Some("avc1.4d401f"), Some("none"), None),
        ];
        let br = std::collections::HashMap::new();
        let mpd =
            synthesize_manifest(secret, "vid", &formats, Some(60.0), &br).expect("synthesize");
        assert!(!mpd.contains(r#"id="160""#), "144p must be dropped:\n{mpd}");
        assert!(!mpd.contains(r#"id="133""#), "240p must be dropped:\n{mpd}");
        assert!(mpd.contains(r#"id="134""#), "360p must survive:\n{mpd}");
        assert!(mpd.contains(r#"id="136""#), "720p must survive:\n{mpd}");
    }

    /// Muxed formats (vcodec AND acodec set, like itag 18) duplicate
    /// content already in the adaptive video/audio AdaptationSets and
    /// confuse dash.js's track selection. Drop them entirely.
    #[test]
    fn synthesize_manifest_drops_muxed_formats() {
        let secret = b"secret-aaaaaaaaaaaaaaaaaaaaaaaa";
        let mut muxed = https_format(
            "18",
            Some(360),
            Some("avc1.42001E"),
            Some("mp4a.40.2"),
            None,
        );
        // Make sure this is genuinely muxed (both codecs set, height
        // present). yt-dlp tags itag 18 this way.
        muxed.acodec = Some("mp4a.40.2".into());
        let formats = vec![
            muxed,
            https_format("136", Some(720), Some("avc1.4d401f"), Some("none"), None),
            https_format("140", None, Some("none"), Some("mp4a.40.2"), Some("en")),
        ];
        let br = std::collections::HashMap::new();
        let mpd =
            synthesize_manifest(secret, "vid", &formats, Some(60.0), &br).expect("synthesize");
        assert!(!mpd.contains(r#"id="18""#), "muxed format dropped:\n{mpd}");
        assert!(mpd.contains(r#"id="136""#), "video-only kept:\n{mpd}");
        assert!(mpd.contains(r#"id="140""#), "audio-only kept:\n{mpd}");
    }

    /// Audio Representations: prefer mp4a/AAC over opus for the same
    /// language, and drop DRC variants entirely.
    #[test]
    fn synthesize_manifest_picks_one_audio_codec_per_language() {
        let secret = b"secret-aaaaaaaaaaaaaaaaaaaaaaaa";
        let mut drc = https_format("251-drc", None, Some("none"), Some("opus"), Some("en"));
        drc.format_note = Some("DRC, low".into());
        let formats = vec![
            // Three English candidates: mp4a should win, drc dropped.
            https_format("251", None, Some("none"), Some("opus"), Some("en")),
            https_format("140", None, Some("none"), Some("mp4a.40.2"), Some("en")),
            drc,
            // Spanish has only opus — it should survive even though
            // it's not mp4a.
            https_format("251-es", None, Some("none"), Some("opus"), Some("es")),
        ];
        let br = std::collections::HashMap::new();
        let mpd =
            synthesize_manifest(secret, "vid", &formats, Some(60.0), &br).expect("synthesize");
        assert!(
            mpd.contains(r#"id="140""#),
            "english AAC should win:\n{mpd}"
        );
        assert!(!mpd.contains(r#"id="251""#), "english opus dropped:\n{mpd}");
        assert!(
            !mpd.contains(r#"id="251-drc""#),
            "DRC variant dropped:\n{mpd}"
        );
        assert!(
            mpd.contains(r#"id="251-es""#),
            "spanish opus survives (only candidate):\n{mpd}"
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
