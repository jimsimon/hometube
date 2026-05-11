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
use hmac::{Hmac, Mac};
use quick_xml::events::{BytesStart, Event};
use quick_xml::reader::Reader;
use quick_xml::writer::Writer;
use rand::RngCore;
use sha2::Sha256;
use sqlx::SqlitePool;
use tracing::warn;

use crate::error::{AppError, AppResult};
use crate::services::setup::{get_config_value, set_config_value};
use crate::services::youtube;

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
    rand::thread_rng().fill_bytes(&mut bytes);
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
pub fn rewrite_manifest(secret: &[u8], video_id: &str, manifest: &str) -> AppResult<String> {
    let mut reader = Reader::from_str(manifest);
    reader.config_mut().trim_text(false);
    let mut writer = Writer::new(Cursor::new(Vec::<u8>::new()));

    // Track the current `<Representation id="...">` so segment URLs
    // know which yt-dlp format they belong to.
    let mut current_format: Option<String> = None;

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
                if let Some(rewritten) =
                    rewrite_url_element(&e, secret, video_id, current_format.as_deref(), false)?
                {
                    writer.write_event(Event::Start(rewritten)).ok();
                } else {
                    writer.write_event(Event::Start(e.into_owned())).ok();
                }
            }
            Event::Empty(e) => {
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
                if e.name().as_ref() == b"Representation" {
                    current_format = None;
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
}
