//! HLS (HTTP Live Streaming) manifest rewriting.
//!
//! YouTube serves some videos with an HLS master playlist instead of (or
//! in addition to) a DASH manifest. yt-dlp's per-format `manifest_url`
//! field for any format with `protocol = "m3u8_native"` points at one of
//! these master playlists; the body looks like:
//!
//! ```text
//! #EXTM3U
//! #EXT-X-INDEPENDENT-SEGMENTS
//! #EXT-X-STREAM-INF:BANDWIDTH=183677,CODECS="mp4a.40.5,avc1.4D400C",...
//! https://manifest.googlevideo.com/api/manifest/hls_playlist/...index.m3u8
//! ...
//! ```
//!
//! Each variant URL points to a *media* playlist that lists individual
//! `.ts` (or `.m4s`) segment URLs on `*.googlevideo.com`. Browsers can't
//! fetch those directly because of CORS and YouTube's session-binding
//! requirements, so HomeTube proxies the entire chain:
//!
//! ```text
//! Master playlist  → /api/proxy/hls?kind=playlist&url=<signed>
//! Media playlist   → /api/proxy/hls?kind=segment&url=<signed>
//! Each segment     ↗
//! ```
//!
//! The signature ([`crate::services::dash::sign_query`]) prevents abuse:
//! clients cannot construct proxy URLs for arbitrary upstream targets
//! without the server-side HMAC secret.

use crate::services::dash::{sign_query, verify_query};
use crate::services::youtube;

/// Returns true iff `body` looks like an HLS playlist (m3u8). HLS files
/// always start with the `#EXTM3U` tag per RFC 8216 §4.3.1.1.
pub fn is_hls_manifest(body: &str) -> bool {
    body.trim_start().starts_with("#EXTM3U")
}

/// What sort of upstream URL is being proxied: a nested playlist (which
/// itself needs its segment URLs rewritten before being returned) or a
/// raw media segment (passed through unchanged).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HlsProxyKind {
    Playlist,
    Segment,
}

impl HlsProxyKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            HlsProxyKind::Playlist => "playlist",
            HlsProxyKind::Segment => "segment",
        }
    }

    pub fn from_str(s: &str) -> Option<Self> {
        match s {
            "playlist" => Some(HlsProxyKind::Playlist),
            "segment" => Some(HlsProxyKind::Segment),
            _ => None,
        }
    }
}

/// Build a signed `/api/proxy/hls?...` URL for the given upstream
/// playlist/segment. The signature is over the canonical
/// (kind, url, video_id) tuple so the verifier can recover it exactly.
pub fn build_proxy_url(
    secret: &[u8],
    video_id: &str,
    kind: HlsProxyKind,
    upstream_url: &str,
) -> String {
    let params: Vec<(&str, String)> = vec![
        ("video_id", video_id.to_string()),
        ("kind", kind.as_str().to_string()),
        ("url", upstream_url.to_string()),
    ];
    let sig = sign_query(secret, &params);
    format!(
        "/api/proxy/hls?video_id={}&kind={}&url={}&sig={}",
        youtube::percent_encode(video_id),
        kind.as_str(),
        youtube::percent_encode(upstream_url),
        sig,
    )
}

/// Verify the parameters of an incoming `/api/proxy/hls` request.
pub fn verify_proxy_params(
    secret: &[u8],
    video_id: &str,
    kind: HlsProxyKind,
    upstream_url: &str,
    sig: &str,
) -> bool {
    let params: Vec<(&str, String)> = vec![
        ("video_id", video_id.to_string()),
        ("kind", kind.as_str().to_string()),
        ("url", upstream_url.to_string()),
    ];
    verify_query(secret, &params, sig)
}

/// Rewrite an HLS playlist (master *or* media), replacing every absolute
/// URL line with a signed proxy URL.
///
/// Per RFC 8216, lines beginning with `#` are tags/comments and lines
/// not beginning with `#` are URIs (or empty). For a master playlist
/// the URIs are nested playlists; for a media playlist they are media
/// segments. The caller picks the right [`HlsProxyKind`] for the URI
/// lines they expect to see.
///
/// Lines that are not absolute `http(s)://` URLs (relative refs, blank
/// lines, comments) are passed through unchanged; HLS supports relative
/// URIs but YouTube's manifests only emit absolute URLs.
pub fn rewrite_playlist(
    secret: &[u8],
    video_id: &str,
    body: &str,
    uri_kind: HlsProxyKind,
) -> String {
    let mut out = String::with_capacity(body.len() + 1024);
    for line in body.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            out.push_str(line);
            out.push('\n');
            continue;
        }
        if trimmed.starts_with("http://") || trimmed.starts_with("https://") {
            let proxied = build_proxy_url(secret, video_id, uri_kind, trimmed);
            out.push_str(&proxied);
        } else {
            // Relative / unknown — leave alone. (Won't work in the
            // browser without a base URL, but better than corrupting
            // it.)
            out.push_str(line);
        }
        out.push('\n');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn secret() -> Vec<u8> {
        vec![0x42; 32]
    }

    #[test]
    fn detects_hls() {
        assert!(is_hls_manifest("#EXTM3U\n#EXT-X-VERSION:3\n"));
        assert!(is_hls_manifest("\n  #EXTM3U\nfoo"));
        assert!(!is_hls_manifest("<?xml version=\"1.0\"?>\n<MPD>"));
        assert!(!is_hls_manifest(""));
    }

    #[test]
    fn rewrites_master_variant_urls() {
        let body = "\
#EXTM3U
#EXT-X-INDEPENDENT-SEGMENTS
#EXT-X-STREAM-INF:BANDWIDTH=183677,RESOLUTION=256x144
https://manifest.googlevideo.com/api/manifest/hls_playlist/expire/1/playlist/index.m3u8
#EXT-X-STREAM-INF:BANDWIDTH=350372,RESOLUTION=426x240
https://manifest.googlevideo.com/api/manifest/hls_playlist/expire/2/playlist/index.m3u8
";
        let rewritten = rewrite_playlist(&secret(), "abc123", body, HlsProxyKind::Playlist);
        // Each variant URL replaced with /api/proxy/hls?...
        assert_eq!(rewritten.matches("/api/proxy/hls?").count(), 2);
        assert_eq!(rewritten.matches("kind=playlist").count(), 2);
        // Tag lines preserved verbatim.
        assert!(rewritten.contains("#EXT-X-STREAM-INF:BANDWIDTH=183677"));
        // The original URLs only appear as percent-encoded values inside
        // the proxy `url=` parameter — never as bare lines that would
        // make the browser fetch them directly.
        for line in rewritten.lines() {
            assert!(
                !line.starts_with("https://manifest.googlevideo.com"),
                "found bare upstream URL line: {line}",
            );
        }
    }

    #[test]
    fn rewrites_media_segment_urls() {
        let body = "\
#EXTM3U
#EXT-X-VERSION:3
#EXT-X-TARGETDURATION:6
#EXTINF:6.0,
https://r1.googlevideo.com/seg/1.ts
#EXTINF:6.0,
https://r1.googlevideo.com/seg/2.ts
#EXT-X-ENDLIST
";
        let rewritten = rewrite_playlist(&secret(), "abc123", body, HlsProxyKind::Segment);
        assert_eq!(rewritten.matches("/api/proxy/hls?").count(), 2);
        assert_eq!(rewritten.matches("kind=segment").count(), 2);
        assert!(rewritten.contains("#EXT-X-ENDLIST"));
    }

    #[test]
    fn proxy_url_round_trips() {
        let url = "https://r1.googlevideo.com/seg/1.ts?foo=bar&baz=qux";
        let proxied = build_proxy_url(&secret(), "vid", HlsProxyKind::Segment, url);
        // Pull out sig and url from the proxied URL.
        let sig = proxied
            .split("&sig=")
            .nth(1)
            .expect("sig field present")
            .to_string();
        assert!(verify_proxy_params(
            &secret(),
            "vid",
            HlsProxyKind::Segment,
            url,
            &sig
        ));
        // Tampering with kind invalidates the signature.
        assert!(!verify_proxy_params(
            &secret(),
            "vid",
            HlsProxyKind::Playlist,
            url,
            &sig
        ));
        // Tampering with video_id invalidates the signature.
        assert!(!verify_proxy_params(
            &secret(),
            "other-vid",
            HlsProxyKind::Segment,
            url,
            &sig
        ));
    }
}
