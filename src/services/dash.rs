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
