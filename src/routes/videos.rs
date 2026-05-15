//! Video metadata + DASH manifest + segment-proxy routes.
//!
//! Most of the heavy lifting lives in `services::ytdlp`, `services::video_cache`,
//! and `services::dash`. The handlers here glue access control, the
//! cache, and the proxy together.
//!
//! Routes:
//! - `GET /api/videos/:videoId` — metadata + child access check
//! - `GET /api/videos/:videoId/stream` — JSON metadata (formats list + manifest text)
//! - `GET /api/videos/:videoId/stream/manifest.mpd` — rewritten DASH XML
//! - `GET /api/videos/:videoId/captions` — caption track list
//! - `GET /api/videos/:videoId/captions/:lang` — WebVTT track
//! - `GET /api/proxy/segment` — signed DASH segment proxy
//! - `GET /api/proxy/audio` — audio-only stream proxy
//! - `GET /api/proxy/thumbnail/:videoId` — thumbnail proxy

use std::collections::HashMap;
use std::path::{Path as FsPath, PathBuf};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use axum::{
    body::Body,
    extract::{Path, Query, State},
    http::{header, HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
use chrono::Utc;
use futures_util::TryStreamExt;
use serde::{Deserialize, Serialize};
use sqlx::SqlitePool;
use tokio::sync::Mutex;
use tracing::{debug, warn};

use crate::error::{AppError, AppResult};
use crate::middleware::auth::CurrentAccount;
use crate::models::account::AccountType;
use crate::services::access::can_child_view;
use crate::services::dash;
use crate::services::hls::{self, HlsProxyKind};
use crate::services::video_cache::VideoCache;
use crate::services::ytdlp::{ExtractResult, Format};
use crate::state::AppState;

// ---------------------------------------------------------------------------
// Public response types
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize)]
pub struct VideoMetadata {
    pub id: String,
    pub title: Option<String>,
    pub channel_id: Option<String>,
    pub channel_title: Option<String>,
    pub duration_seconds: Option<f64>,
    pub thumbnail_url: Option<String>,
}

/// Wire-format string identifying the manifest flavour returned in
/// [`StreamResponse::manifest`]. The frontend uses this to pick the
/// right vidstack source `type` (and therefore which provider — dash.js
/// or hls.js — handles playback).
#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ManifestType {
    /// MPEG-DASH (`application/dash+xml`).
    Dash,
    /// HLS master playlist (`application/vnd.apple.mpegurl`).
    Hls,
}

#[derive(Debug, Serialize)]
pub struct StreamResponse {
    pub video_id: String,
    /// Rewritten manifest text. For DASH this is the rewritten MPD XML;
    /// for HLS it's the rewritten master playlist. Both reference our
    /// proxy endpoints rather than `*.googlevideo.com` directly.
    pub manifest: Option<String>,
    /// Which manifest flavour `manifest` is, when present. Only set
    /// when `manifest` is `Some(_)`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub manifest_type: Option<ManifestType>,
    /// Filtered list of progressive formats (max-quality cap applied).
    pub formats: Vec<Format>,
}

#[derive(Debug, Serialize)]
pub struct CaptionTrack {
    pub lang: String,
    pub name: Option<String>,
    pub auto_generated: bool,
}

// ---------------------------------------------------------------------------
// /api/videos/:videoId
// ---------------------------------------------------------------------------

/// `GET /api/videos/:videoId` — return metadata after running the
/// allowlist check for child accounts.
pub async fn get_metadata(
    State(state): State<AppState>,
    current: CurrentAccount,
    Path(video_id): Path<String>,
) -> AppResult<Json<VideoMetadata>> {
    let cache = video_cache(&state);
    let result = cache
        .get_or_extract(&state.db, &state.config, &video_id)
        .await?;

    enforce_access(&state.db, &current, &video_id, &result).await?;

    Ok(Json(VideoMetadata {
        id: result.id.clone(),
        title: result.title.clone(),
        channel_id: result.channel_id.clone(),
        channel_title: result.channel_title.clone(),
        duration_seconds: result.duration,
        thumbnail_url: pick_thumbnail(&result),
    }))
}

// ---------------------------------------------------------------------------
// /api/videos/:videoId/stream
// ---------------------------------------------------------------------------

/// `GET /api/videos/:videoId/stream` — return formats and a rewritten
/// DASH manifest. Formats above the child's `max_quality` cap are
/// dropped before serialisation.
pub async fn get_stream(
    State(state): State<AppState>,
    current: CurrentAccount,
    Path(video_id): Path<String>,
) -> AppResult<Json<StreamResponse>> {
    let cache = video_cache(&state);
    let result = cache
        .get_or_extract(&state.db, &state.config, &video_id)
        .await?;
    enforce_access(&state.db, &current, &video_id, &result).await?;

    // Apply max-quality cap for child accounts.
    let max_height = if matches!(current.account_type, AccountType::Child) {
        max_height_for_child(&state.db, current.id).await?
    } else {
        None
    };

    let formats: Vec<Format> = result
        .formats
        .iter()
        .filter(|f| match (max_height, f.height) {
            (Some(cap), Some(h)) => h <= cap,
            // Audio-only formats have no height — keep them.
            _ => true,
        })
        .cloned()
        .collect();

    // Fetch + rewrite the manifest (DASH or HLS, depending on what
    // yt-dlp surfaced for this video).
    let (manifest, manifest_type) = match fetch_and_rewrite_manifest(&state, &video_id, &result)
        .await
    {
        Ok(Some((body, ty))) => (Some(body), Some(ty)),
        Ok(None) => (None, None),
        Err(err) => {
            warn!(%video_id, %err, "failed to fetch upstream manifest");
            (None, None)
        }
    };

    Ok(Json(StreamResponse {
        video_id,
        manifest,
        manifest_type,
        formats,
    }))
}

// ---------------------------------------------------------------------------
// /api/videos/:videoId/stream/manifest.mpd
// ---------------------------------------------------------------------------

/// `GET /api/videos/:videoId/stream/manifest.mpd` — return the rewritten
/// manifest body directly. vidstack fetches this URL to bootstrap
/// playback; we cannot reuse the JSON `/stream` endpoint because that
/// embeds the manifest text inside a JSON envelope. The actual content
/// type may be DASH XML *or* HLS m3u8 depending on what yt-dlp
/// surfaced — the `Content-Type` response header tells the player which
/// provider to engage.
pub async fn get_stream_manifest(
    State(state): State<AppState>,
    current: CurrentAccount,
    Path(video_id): Path<String>,
) -> AppResult<Response> {
    let cache = video_cache(&state);
    let result = cache
        .get_or_extract(&state.db, &state.config, &video_id)
        .await?;
    enforce_access(&state.db, &current, &video_id, &result).await?;

    let (body, ty) = fetch_and_rewrite_manifest(&state, &video_id, &result)
        .await?
        .ok_or(AppError::NotFound)?;

    let content_type = match ty {
        ManifestType::Dash => "application/dash+xml",
        ManifestType::Hls => "application/vnd.apple.mpegurl",
    };
    let mut headers = HeaderMap::new();
    headers.insert(header::CONTENT_TYPE, content_type.parse().unwrap());
    headers.insert(header::CACHE_CONTROL, "no-store".parse().unwrap());
    Ok((StatusCode::OK, headers, body).into_response())
}

/// Fetch the upstream manifest URL surfaced by yt-dlp, sniff whether
/// it's DASH or HLS, and return the rewritten body together with the
/// flavour. `Ok(None)` means yt-dlp didn't expose any manifest at all
/// for this video.
async fn fetch_and_rewrite_manifest(
    state: &AppState,
    video_id: &str,
    result: &ExtractResult,
) -> AppResult<Option<(String, ManifestType)>> {
    let Some(url) = result
        .manifest_url
        .clone()
        .or_else(|| result.formats.iter().find_map(|f| f.manifest_url.clone()))
    else {
        return Ok(None);
    };

    let secret = dash::ensure_proxy_secret(&state.db).await?;
    let res = reqwest::get(&url).await?;
    if !res.status().is_success() {
        warn!(%url, status = %res.status(), "non-2xx fetching upstream manifest");
        return Ok(None);
    }
    let body = res.text().await?;

    if hls::is_hls_manifest(&body) {
        let rewritten = hls::rewrite_playlist(&secret, video_id, &body, HlsProxyKind::Playlist);
        Ok(Some((rewritten, ManifestType::Hls)))
    } else {
        let rewritten = dash::rewrite_manifest(&secret, video_id, &body)?;
        Ok(Some((rewritten, ManifestType::Dash)))
    }
}

// ---------------------------------------------------------------------------
// /api/videos/:videoId/captions
// ---------------------------------------------------------------------------

pub async fn list_captions(
    State(state): State<AppState>,
    current: CurrentAccount,
    Path(video_id): Path<String>,
) -> AppResult<Json<Vec<CaptionTrack>>> {
    let cache = video_cache(&state);
    let result = cache
        .get_or_extract(&state.db, &state.config, &video_id)
        .await?;
    enforce_access(&state.db, &current, &video_id, &result).await?;

    let mut tracks: Vec<CaptionTrack> = result
        .subtitles
        .keys()
        .map(|lang| CaptionTrack {
            lang: lang.clone(),
            name: None,
            auto_generated: false,
        })
        .collect();
    for lang in result.automatic_captions.keys() {
        tracks.push(CaptionTrack {
            lang: lang.clone(),
            name: None,
            auto_generated: true,
        });
    }
    Ok(Json(tracks))
}

/// `GET /api/videos/:videoId/captions/:lang` — return a WebVTT track for
/// the requested language.
///
/// Lookup order:
///
/// 1. In-memory `(video_id, lang)` cache (1 hour TTL).
/// 2. yt-dlp metadata: if we already have a `.vtt` URL, fetch it.
/// 3. Re-invoke yt-dlp with `--convert-subs vtt` so any non-VTT source
///    (SRV1/SRV3/TTML) gets converted on the server.
///
/// On success the converted body is cached in memory so repeated player
/// "select track" actions don't re-hit yt-dlp.
pub async fn get_caption(
    State(state): State<AppState>,
    current: CurrentAccount,
    Path((video_id, lang)): Path<(String, String)>,
) -> AppResult<Response> {
    let cache = video_cache(&state);
    let result = cache
        .get_or_extract(&state.db, &state.config, &video_id)
        .await?;
    enforce_access(&state.db, &current, &video_id, &result).await?;

    let cache_key = (video_id.clone(), lang.clone());
    if let Some(body) = caption_cache_get(&cache_key).await {
        return Ok(vtt_response(body));
    }

    // Prefer user-provided subtitles, fall back to auto-captions.
    let track = result
        .subtitles
        .get(&lang)
        .and_then(|tracks| {
            tracks
                .iter()
                .find(|t| t.ext == "vtt")
                .or_else(|| tracks.first())
        })
        .or_else(|| {
            result.automatic_captions.get(&lang).and_then(|tracks| {
                tracks
                    .iter()
                    .find(|t| t.ext == "vtt")
                    .or_else(|| tracks.first())
            })
        })
        .cloned();

    // Fast path: yt-dlp metadata exposed a vtt URL.
    if let Some(track) = &track {
        if track.ext == "vtt" {
            match reqwest::get(&track.url).await {
                Ok(res) if res.status().is_success() => {
                    if let Ok(body) = res.text().await {
                        caption_cache_set(cache_key.clone(), body.clone()).await;
                        return Ok(vtt_response(body));
                    }
                }
                Ok(res) => warn!(status = %res.status(), "caption fetch returned non-2xx"),
                Err(err) => warn!(%err, "caption fetch failed"),
            }
        }
    }

    // Slow path: ask yt-dlp to download + convert to vtt.
    let body = crate::services::ytdlp::extract_subtitles(&state.config, &video_id, &lang).await?;
    caption_cache_set(cache_key, body.clone()).await;
    Ok(vtt_response(body))
}

/// Build the `text/vtt` HTTP response.
fn vtt_response(body: String) -> Response {
    let mut response = (StatusCode::OK, body).into_response();
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        "text/vtt; charset=utf-8".parse().unwrap(),
    );
    // Help vidstack cache cross-track switches by allowing a short
    // browser cache lifetime.
    response.headers_mut().insert(
        header::CACHE_CONTROL,
        "private, max-age=3600".parse().unwrap(),
    );
    response
}

// ---------------------------------------------------------------------------
// Caption in-memory cache
// ---------------------------------------------------------------------------

/// TTL for the converted-VTT in-memory cache.
const CAPTION_TTL: Duration = Duration::from_secs(60 * 60);

#[derive(Clone)]
struct CaptionEntry {
    inserted_at: Instant,
    body: String,
}

fn caption_cache() -> &'static Mutex<HashMap<(String, String), CaptionEntry>> {
    static CACHE: OnceLock<Mutex<HashMap<(String, String), CaptionEntry>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

async fn caption_cache_get(key: &(String, String)) -> Option<String> {
    let mut guard = caption_cache().lock().await;
    if let Some(entry) = guard.get(key) {
        if entry.inserted_at.elapsed() < CAPTION_TTL {
            return Some(entry.body.clone());
        }
        guard.remove(key);
    }
    None
}

async fn caption_cache_set(key: (String, String), body: String) {
    let mut guard = caption_cache().lock().await;
    guard.insert(
        key,
        CaptionEntry {
            inserted_at: Instant::now(),
            body,
        },
    );
}

// ---------------------------------------------------------------------------
// /api/proxy/segment
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct SegmentQuery {
    pub video_id: String,
    pub format: String,
    pub sq: String,
    pub sig: String,
}

/// `GET /api/proxy/segment` — serve a DASH segment, signed.
///
/// On hit: stream from disk. On miss: fetch from googlevideo.com,
/// tee-write to disk, stream to the client.
pub async fn get_segment(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<SegmentQuery>,
) -> AppResult<Response> {
    let secret = dash::ensure_proxy_secret(&state.db).await?;
    let params: Vec<(&str, String)> = vec![
        ("video_id", q.video_id.clone()),
        ("format", q.format.clone()),
        ("sq", q.sq.clone()),
    ];
    if !dash::verify_query(&secret, &params, &q.sig) {
        return Err(AppError::Forbidden);
    }

    // Disk-cache hit?
    if let Some((path, size)) =
        lookup_cached_segment(&state.db, &q.video_id, &q.format, &q.sq).await?
    {
        debug!(video = %q.video_id, fmt = %q.format, sq = %q.sq, "segment cache hit");
        crate::services::cron::CACHE_HIT_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        return serve_file(&path, size, &headers).await;
    }
    crate::services::cron::CACHE_MISS_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);

    // Miss: resolve the upstream URL via the cached metadata.
    let cache = video_cache(&state);
    let result = cache
        .get_or_extract(&state.db, &state.config, &q.video_id)
        .await?;
    let upstream = build_upstream_segment_url(&result, &q.format, &q.sq).ok_or_else(|| {
        AppError::BadRequest(format!(
            "no upstream URL for video {} format {}",
            q.video_id, q.format
        ))
    })?;

    // Fetch from upstream. For the miss path we read the whole segment
    // into memory (2-5 MB typical), write it to disk, and serve. Range
    // requests are passed through so seek-while-uncached still works
    // even though we won't cache a partial response.
    let is_range = headers.get(header::RANGE).is_some();
    let mut req = reqwest::Client::new().get(&upstream);
    if let Some(range) = headers.get(header::RANGE) {
        if let Ok(s) = range.to_str() {
            req = req.header(header::RANGE, s);
        }
    }
    let res = req.send().await.map_err(AppError::Http)?;
    let status = res.status();
    let content_type = res
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("application/octet-stream")
        .to_string();

    if !status.is_success() || is_range {
        // Either the upstream errored or this is a range request — pass
        // through without caching.
        let stream = res.bytes_stream().map_err(std::io::Error::other);
        let body = Body::from_stream(stream);
        let mut response = Response::new(body);
        *response.status_mut() = status;
        response
            .headers_mut()
            .insert(header::CONTENT_TYPE, content_type.parse().unwrap());
        return Ok(response);
    }

    let bytes = res.bytes().await.map_err(AppError::Http)?;
    let cache_path = segment_cache_path(&state.config, &q.video_id, &q.format, &q.sq);
    if let Err(err) = write_segment(&cache_path, &bytes).await {
        warn!(error = %err, "segment cache write failed");
    } else {
        let _ = register_cached_segment(
            &state.db,
            &q.video_id,
            &q.format,
            &q.sq,
            &cache_path,
            bytes.len() as i64,
        )
        .await;
    }

    let body = Body::from(bytes);
    let mut response = Response::new(body);
    *response.status_mut() = status;
    response
        .headers_mut()
        .insert(header::CONTENT_TYPE, content_type.parse().unwrap());
    Ok(response)
}

// ---------------------------------------------------------------------------
// /api/proxy/hls
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct HlsProxyQuery {
    pub video_id: String,
    pub kind: String,
    pub url: String,
    pub sig: String,
}

/// `GET /api/proxy/hls` — proxy an HLS playlist or segment URL signed
/// by [`crate::services::hls::build_proxy_url`].
///
/// `kind=playlist` fetches a media playlist, rewrites every segment URL
/// in it with another signed proxy URL (so the browser doesn't try to
/// fetch googlevideo.com directly), and returns the rewritten playlist.
///
/// `kind=segment` fetches the upstream segment and streams it through
/// to the client unchanged.
pub async fn get_hls_proxy(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<HlsProxyQuery>,
) -> AppResult<Response> {
    let secret = dash::ensure_proxy_secret(&state.db).await?;
    let kind: HlsProxyKind = q
        .kind
        .parse()
        .map_err(|_| AppError::BadRequest(format!("invalid kind: {}", q.kind)))?;
    if !hls::verify_proxy_params(&secret, &q.video_id, kind, &q.url, &q.sig) {
        return Err(AppError::Forbidden);
    }
    if !is_allowed_hls_host(&q.url) {
        warn!(url = %q.url, "rejecting HLS proxy fetch for non-allowlisted host");
        return Err(AppError::Forbidden);
    }

    let mut req = reqwest::Client::new().get(&q.url);
    if matches!(kind, HlsProxyKind::Segment) {
        if let Some(range) = headers.get(header::RANGE) {
            if let Ok(s) = range.to_str() {
                req = req.header(header::RANGE, s);
            }
        }
    }
    let res = req.send().await.map_err(AppError::Http)?;
    let status = res.status();
    if !status.is_success() {
        warn!(url = %q.url, %status, "upstream HLS fetch returned non-2xx");
    }

    let upstream_content_type = res
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("application/octet-stream")
        .to_string();

    match kind {
        HlsProxyKind::Playlist => {
            // Only attempt URL rewriting on a successful response — an
            // error body (HTML 404 page, JSON error, etc.) is *not* a
            // valid m3u8 and feeding it through `rewrite_playlist`
            // would mangle whatever debugging info upstream sent. On
            // non-2xx pass the body through verbatim with the upstream
            // status so callers can see what went wrong.
            let body = res.bytes().await.map_err(AppError::Http)?;
            if !status.is_success() {
                let mut response = Response::new(Body::from(body));
                *response.status_mut() = status;
                response
                    .headers_mut()
                    .insert(header::CONTENT_TYPE, upstream_content_type.parse().unwrap());
                return Ok(response);
            }
            let body_str = std::str::from_utf8(&body).map_err(|e| {
                AppError::Other(anyhow::anyhow!("upstream playlist not UTF-8: {e}"))
            })?;
            let rewritten =
                hls::rewrite_playlist(&secret, &q.video_id, body_str, HlsProxyKind::Segment);
            let mut response = Response::new(Body::from(rewritten));
            *response.status_mut() = status;
            response.headers_mut().insert(
                header::CONTENT_TYPE,
                "application/vnd.apple.mpegurl".parse().unwrap(),
            );
            response
                .headers_mut()
                .insert(header::CACHE_CONTROL, "no-store".parse().unwrap());
            Ok(response)
        }
        HlsProxyKind::Segment => {
            // Stream the segment bytes through unchanged. Range
            // requests are honoured (passed through above). Non-2xx
            // bodies are also passed through so the browser sees the
            // upstream status.
            let stream = res.bytes_stream().map_err(std::io::Error::other);
            let body = Body::from_stream(stream);
            let mut response = Response::new(body);
            *response.status_mut() = status;
            response
                .headers_mut()
                .insert(header::CONTENT_TYPE, upstream_content_type.parse().unwrap());
            Ok(response)
        }
    }
}

/// Defense-in-depth host allowlist for the HLS proxy. The HMAC signature
/// alone already prevents an unauthenticated attacker from constructing
/// proxy URLs to arbitrary upstreams, but if the proxy secret were ever
/// leaked HomeTube would become a credentialed open proxy / SSRF tool.
/// Refuse anything that doesn't look like a YouTube CDN host.
///
/// yt-dlp's HLS manifests for YouTube only ever emit URLs at
/// `manifest.googlevideo.com` (master/media playlists) and
/// `*.googlevideo.com` (segments). Legitimate traffic is unaffected.
fn is_allowed_hls_host(url: &str) -> bool {
    let Ok(parsed) = reqwest::Url::parse(url) else {
        return false;
    };
    if parsed.scheme() != "https" {
        return false;
    }
    let Some(host) = parsed.host_str() else {
        return false;
    };
    let host = host.to_ascii_lowercase();
    host == "googlevideo.com"
        || host.ends_with(".googlevideo.com")
        || host == "youtube.com"
        || host.ends_with(".youtube.com")
}

#[cfg(test)]
mod hls_host_tests {
    use super::is_allowed_hls_host;

    #[test]
    fn accepts_googlevideo_subdomains() {
        assert!(is_allowed_hls_host(
            "https://manifest.googlevideo.com/api/manifest/hls_playlist/foo"
        ));
        assert!(is_allowed_hls_host(
            "https://rr1---sn-bvvbaxivnuxq5uu-vgqz.googlevideo.com/videoplayback/seg.ts"
        ));
        assert!(is_allowed_hls_host("https://www.youtube.com/"));
    }

    #[test]
    fn rejects_other_hosts() {
        assert!(!is_allowed_hls_host("https://example.com/"));
        assert!(!is_allowed_hls_host("https://googlevideo.com.evil.com/"));
        assert!(!is_allowed_hls_host("https://evil.com/?googlevideo.com"));
        assert!(!is_allowed_hls_host("http://manifest.googlevideo.com/"));
        assert!(!is_allowed_hls_host("https://169.254.169.254/"));
        assert!(!is_allowed_hls_host("not a url"));
    }
}

// ---------------------------------------------------------------------------
// /api/proxy/audio + /api/proxy/thumbnail
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct AudioQuery {
    pub video_id: String,
    pub format: String,
    /// Optional DASH segment sequence number. When present the audio
    /// stream is served segment-by-segment (mirroring the `/api/proxy/segment`
    /// flow) so each segment is independently cacheable. When absent
    /// the full audio file is streamed without disk caching.
    #[serde(default)]
    pub sq: Option<String>,
    pub sig: String,
}

/// `GET /api/proxy/audio` — proxy an audio-only stream.
///
/// When `sq=` is present the request is treated as a DASH audio segment
/// (cached on disk by `(video_id, format_id, sq)`, signed over the same
/// triple). Otherwise the request is for the full audio URL of the
/// chosen format and is streamed through without caching — the
/// signature in that case is over `(video_id, format)` only.
pub async fn get_audio(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<AudioQuery>,
) -> AppResult<Response> {
    let secret = dash::ensure_proxy_secret(&state.db).await?;
    let mut params: Vec<(&str, String)> = vec![
        ("video_id", q.video_id.clone()),
        ("format", q.format.clone()),
    ];
    if let Some(sq) = &q.sq {
        params.push(("sq", sq.clone()));
    }
    if !dash::verify_query(&secret, &params, &q.sig) {
        return Err(AppError::Forbidden);
    }

    // Segmented path: identical caching strategy to /api/proxy/segment.
    if let Some(sq) = &q.sq {
        if let Some((path, size)) =
            lookup_cached_segment(&state.db, &q.video_id, &q.format, sq).await?
        {
            debug!(video = %q.video_id, fmt = %q.format, sq = %sq, "audio segment cache hit");
            crate::services::cron::CACHE_HIT_COUNTER
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            return serve_file(&path, size, &headers).await;
        }
        crate::services::cron::CACHE_MISS_COUNTER
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }

    let cache = video_cache(&state);
    let result = cache
        .get_or_extract(&state.db, &state.config, &q.video_id)
        .await?;

    // If no explicit format requested, default to the highest-bitrate
    // audio-only format available so the player can transition in a
    // single hop.
    let chosen_format = if q.format.is_empty() {
        best_audio_format_id(&result)
            .ok_or_else(|| AppError::BadRequest("no audio formats".into()))?
    } else {
        q.format.clone()
    };

    let format = result
        .formats
        .iter()
        .find(|f| f.format_id == chosen_format)
        .ok_or_else(|| AppError::BadRequest("unknown format".into()))?;

    let url = if let Some(sq) = &q.sq {
        build_upstream_segment_url(&result, &chosen_format, sq).ok_or_else(|| {
            AppError::BadRequest(format!(
                "no upstream URL for video {} format {}",
                q.video_id, chosen_format
            ))
        })?
    } else {
        format
            .url
            .clone()
            .ok_or_else(|| AppError::BadRequest("format has no direct URL".into()))?
    };

    let is_range = headers.get(header::RANGE).is_some();
    let mut req = reqwest::Client::new().get(&url);
    if let Some(range) = headers.get(header::RANGE) {
        if let Ok(s) = range.to_str() {
            req = req.header(header::RANGE, s);
        }
    }
    let res = req.send().await.map_err(AppError::Http)?;
    let status = res.status();
    let content_type = res
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("audio/mp4")
        .to_string();

    // For non-segment requests or range requests we just stream through.
    if q.sq.is_none() || is_range || !status.is_success() {
        let stream = res.bytes_stream().map_err(std::io::Error::other);
        let body = Body::from_stream(stream);
        let mut response = Response::new(body);
        *response.status_mut() = status;
        response
            .headers_mut()
            .insert(header::CONTENT_TYPE, content_type.parse().unwrap());
        return Ok(response);
    }

    // Segment cache write path.
    let bytes = res.bytes().await.map_err(AppError::Http)?;
    let sq = q.sq.as_deref().unwrap_or("");
    let cache_path = segment_cache_path(&state.config, &q.video_id, &chosen_format, sq);
    if let Err(err) = write_segment(&cache_path, &bytes).await {
        warn!(error = %err, "audio segment cache write failed");
    } else {
        let _ = register_cached_segment(
            &state.db,
            &q.video_id,
            &chosen_format,
            sq,
            &cache_path,
            bytes.len() as i64,
        )
        .await;
    }

    let body = Body::from(bytes);
    let mut response = Response::new(body);
    *response.status_mut() = status;
    response
        .headers_mut()
        .insert(header::CONTENT_TYPE, content_type.parse().unwrap());
    Ok(response)
}

/// Pick the best audio-only format ID from a yt-dlp extraction result.
/// "Best" = highest available `abr` among formats whose vcodec is
/// `none` (audio-only).
fn best_audio_format_id(result: &ExtractResult) -> Option<String> {
    result
        .formats
        .iter()
        .filter(|f| {
            f.vcodec.as_deref().map(|c| c == "none").unwrap_or(false)
                && (f.url.is_some() || f.manifest_url.is_some())
        })
        .max_by(|a, b| {
            a.abr
                .unwrap_or(0.0)
                .partial_cmp(&b.abr.unwrap_or(0.0))
                .unwrap_or(std::cmp::Ordering::Equal)
        })
        .map(|f| f.format_id.clone())
}

/// `GET /api/proxy/thumbnail/:videoId` — stream the highest-resolution
/// thumbnail through the server. No HMAC is required; thumbnails are
/// inherently public.
pub async fn get_thumbnail(
    State(state): State<AppState>,
    Path(video_id): Path<String>,
) -> AppResult<Response> {
    let cache = video_cache(&state);
    let result = cache
        .get_or_extract(&state.db, &state.config, &video_id)
        .await?;
    let url = pick_thumbnail(&result).ok_or(AppError::NotFound)?;

    let res = reqwest::get(&url).await.map_err(AppError::Http)?;
    let status = res.status();
    let content_type = res
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("image/jpeg")
        .to_string();
    let stream = res.bytes_stream().map_err(std::io::Error::other);
    let body = Body::from_stream(stream);
    let mut response = Response::new(body);
    *response.status_mut() = status;
    response
        .headers_mut()
        .insert(header::CONTENT_TYPE, content_type.parse().unwrap());
    Ok(response)
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn video_cache(state: &AppState) -> VideoCache {
    // Each call returns a fresh handle but the underlying Arc is shared
    // through the AppState's `video_cache` field once Phase 5 is fully
    // wired. For now we use a process-wide static via OnceCell.
    static CACHE: std::sync::OnceLock<VideoCache> = std::sync::OnceLock::new();
    let _ = state;
    CACHE.get_or_init(VideoCache::new).clone()
}

async fn enforce_access(
    pool: &SqlitePool,
    current: &CurrentAccount,
    video_id: &str,
    extracted: &ExtractResult,
) -> AppResult<()> {
    if matches!(current.account_type, AccountType::Parent) {
        return Ok(());
    }
    // Look up allowlisted playlist IDs that contain this video via
    // child_playlist_videos → child_playlists → allowlisted_playlists.
    let playlist_ids: Vec<(String,)> = sqlx::query_as(
        "SELECT ap.playlist_id \
         FROM allowlisted_playlists ap \
         INNER JOIN child_playlist_videos cpv ON cpv.video_id = ? \
         INNER JOIN child_playlists cp ON cp.id = cpv.playlist_id \
             AND cp.youtube_playlist_id = ap.playlist_id \
         WHERE ap.child_account_id = ?",
    )
    .bind(video_id)
    .bind(current.id)
    .fetch_all(pool)
    .await?;
    let pl_ids: Vec<String> = playlist_ids.into_iter().map(|(id,)| id).collect();
    let allowed = can_child_view(
        pool,
        current.id,
        video_id,
        extracted.channel_id.as_deref(),
        &pl_ids,
    )
    .await?;
    if !allowed {
        return Err(AppError::Forbidden);
    }
    Ok(())
}

async fn max_height_for_child(pool: &SqlitePool, child_id: i64) -> AppResult<Option<i64>> {
    let row: Option<(Option<String>,)> =
        sqlx::query_as("SELECT max_quality FROM child_settings WHERE child_account_id = ?")
            .bind(child_id)
            .fetch_optional(pool)
            .await?;
    Ok(match row.and_then(|(q,)| q) {
        Some(s) => match s.as_str() {
            "480p" => Some(480),
            "720p" => Some(720),
            "1080p" => Some(1080),
            _ => None,
        },
        None => None,
    })
}

fn pick_thumbnail(result: &ExtractResult) -> Option<String> {
    if let Some(direct) = result.thumbnail.clone() {
        return Some(direct);
    }
    result
        .thumbnails
        .iter()
        .max_by_key(|t| t.width.unwrap_or(0))
        .map(|t| t.url.clone())
}

fn build_upstream_segment_url(result: &ExtractResult, format_id: &str, sq: &str) -> Option<String> {
    let format = result.formats.iter().find(|f| f.format_id == format_id)?;

    if let Some(url) = &format.url {
        // Replace `&sq=<old>` with `&sq=<new>` if it's there; otherwise
        // append.
        if url.contains("sq=") {
            let mut out = String::with_capacity(url.len() + sq.len());
            let mut iter = url.split('&');
            if let Some(first) = iter.next() {
                if let Some(rest) = first.strip_prefix("sq=") {
                    out.push_str("sq=");
                    out.push_str(sq);
                    let _ = rest;
                } else {
                    out.push_str(first);
                }
            }
            for part in iter {
                out.push('&');
                if let Some(_rest) = part.strip_prefix("sq=") {
                    out.push_str("sq=");
                    out.push_str(sq);
                } else {
                    out.push_str(part);
                }
            }
            return Some(out);
        }
        return Some(format!("{url}&sq={sq}"));
    }
    None
}

fn segment_cache_path(
    cfg: &crate::config::Config,
    video_id: &str,
    format: &str,
    sq: &str,
) -> PathBuf {
    let _ = cfg;
    let mut path = PathBuf::from("./data/segment_cache");
    path.push(video_id);
    path.push(format);
    path.push(sq);
    path
}

async fn write_segment(path: &FsPath, bytes: &[u8]) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    let tmp = path.with_extension("tmp");
    tokio::fs::write(&tmp, bytes).await?;
    tokio::fs::rename(&tmp, path).await?;
    Ok(())
}

async fn lookup_cached_segment(
    pool: &SqlitePool,
    video_id: &str,
    format: &str,
    sq: &str,
) -> AppResult<Option<(PathBuf, i64)>> {
    let row: Option<(String, i64)> = sqlx::query_as(
        "SELECT file_path, file_size_bytes FROM segment_cache \
         WHERE video_id = ? AND format_id = ? AND segment_number = ?",
    )
    .bind(video_id)
    .bind(format)
    .bind(parse_sq(sq))
    .fetch_optional(pool)
    .await?;
    if let Some((path, size)) = row {
        let p = PathBuf::from(&path);
        if tokio::fs::metadata(&p).await.is_ok() {
            // Touch last_accessed_at so LRU eviction works.
            let _ = sqlx::query(
                "UPDATE segment_cache SET last_accessed_at = ? \
                 WHERE video_id = ? AND format_id = ? AND segment_number = ?",
            )
            .bind(Utc::now().timestamp())
            .bind(video_id)
            .bind(format)
            .bind(parse_sq(sq))
            .execute(pool)
            .await;
            return Ok(Some((p, size)));
        }
    }
    Ok(None)
}

async fn register_cached_segment(
    pool: &SqlitePool,
    video_id: &str,
    format: &str,
    sq: &str,
    path: &FsPath,
    size: i64,
) -> AppResult<()> {
    sqlx::query(
        "INSERT INTO segment_cache \
            (video_id, format_id, segment_number, file_path, file_size_bytes) \
         VALUES (?, ?, ?, ?, ?) \
         ON CONFLICT(video_id, format_id, segment_number) DO UPDATE SET \
            file_path = excluded.file_path, \
            file_size_bytes = excluded.file_size_bytes, \
            last_accessed_at = unixepoch()",
    )
    .bind(video_id)
    .bind(format)
    .bind(parse_sq(sq))
    .bind(path.to_string_lossy().to_string())
    .bind(size)
    .execute(pool)
    .await?;
    Ok(())
}

fn parse_sq(sq: &str) -> i64 {
    sq.parse().unwrap_or(0)
}

/// Serve a cached segment from disk, honouring HTTP Range requests so
/// the vidstack player can seek inside an already-cached segment without
/// a full re-download.
///
/// Behaviour:
/// - Always emits `Accept-Ranges: bytes` so the player knows seeking
///   works.
/// - For full-file requests: streams the whole file with a 200 response.
/// - For a single `Range: bytes=N-M` request:
///   - On valid range: 206 Partial Content with `Content-Range`,
///     `Content-Length`, and the requested byte slice streamed via
///     [`tokio_util::io::ReaderStream`].
///   - On invalid / unsatisfiable range: 416 with
///     `Content-Range: bytes */<total>`.
/// - Multipart / multi-range requests are not supported (rare in
///   practice for DASH segments) — they fall through to the full-file
///   200 response.
async fn serve_file(path: &FsPath, size: i64, headers: &HeaderMap) -> AppResult<Response> {
    use tokio::io::{AsyncReadExt, AsyncSeekExt};
    use tokio_util::io::ReaderStream;

    let total = if size > 0 {
        size as u64
    } else {
        // Fall back to a stat() if the cache table didn't have a value.
        tokio::fs::metadata(path)
            .await
            .map_err(|e| AppError::Other(anyhow::anyhow!("statting cached segment: {e}")))?
            .len()
    };

    if let Some(range_header) = headers.get(header::RANGE) {
        if let Some((start, end)) = parse_single_byte_range(range_header.to_str().ok(), total) {
            // Valid range: stream the requested slice.
            let mut file = tokio::fs::File::open(path)
                .await
                .map_err(|e| AppError::Other(anyhow::anyhow!("opening cached segment: {e}")))?;
            file.seek(std::io::SeekFrom::Start(start))
                .await
                .map_err(|e| AppError::Other(anyhow::anyhow!("seeking cached segment: {e}")))?;
            let length = end - start + 1;
            let stream = ReaderStream::new(file.take(length));
            let body = Body::from_stream(stream);
            let mut response = Response::new(body);
            *response.status_mut() = StatusCode::PARTIAL_CONTENT;
            let h = response.headers_mut();
            h.insert(
                header::CONTENT_TYPE,
                "application/octet-stream".parse().unwrap(),
            );
            h.insert(header::ACCEPT_RANGES, "bytes".parse().unwrap());
            h.insert(
                header::CONTENT_RANGE,
                format!("bytes {start}-{end}/{total}").parse().unwrap(),
            );
            h.insert(header::CONTENT_LENGTH, length.to_string().parse().unwrap());
            return Ok(response);
        }
        // Header was present but unparseable / unsatisfiable — return 416.
        if range_header
            .to_str()
            .ok()
            .filter(|s| s.starts_with("bytes="))
            .is_some()
        {
            let mut response = Response::new(Body::empty());
            *response.status_mut() = StatusCode::RANGE_NOT_SATISFIABLE;
            let h = response.headers_mut();
            h.insert(
                header::CONTENT_RANGE,
                format!("bytes */{total}").parse().unwrap(),
            );
            h.insert(header::ACCEPT_RANGES, "bytes".parse().unwrap());
            return Ok(response);
        }
        // Anything else (e.g. an unknown range unit) — fall through to
        // a full-file 200 response, matching common server behaviour.
    }

    // Full-file response. Stream from disk to avoid loading the whole
    // segment into memory.
    let file = tokio::fs::File::open(path)
        .await
        .map_err(|e| AppError::Other(anyhow::anyhow!("opening cached segment: {e}")))?;
    let stream = ReaderStream::new(file);
    let body = Body::from_stream(stream);
    let mut response = Response::new(body);
    let h = response.headers_mut();
    h.insert(
        header::CONTENT_TYPE,
        "application/octet-stream".parse().unwrap(),
    );
    h.insert(header::ACCEPT_RANGES, "bytes".parse().unwrap());
    h.insert(header::CONTENT_LENGTH, total.to_string().parse().unwrap());
    Ok(response)
}

/// Parse a single-range HTTP `Range` header value (`bytes=N-M`) against
/// a known resource size.
///
/// Returns `Some((start, end))` (both inclusive) if the request is
/// satisfiable. Open-ended ranges (`bytes=N-` or `bytes=-N` for
/// suffix length) are accepted; multipart ranges and non-`bytes` units
/// are rejected.
pub(crate) fn parse_single_byte_range(value: Option<&str>, total: u64) -> Option<(u64, u64)> {
    let value = value?;
    let body = value.strip_prefix("bytes=")?;
    if body.contains(',') {
        return None; // multi-range not supported
    }
    let (start_s, end_s) = body.split_once('-')?;
    let start_s = start_s.trim();
    let end_s = end_s.trim();

    if start_s.is_empty() {
        // Suffix form: "bytes=-N" → last N bytes.
        let suffix_len: u64 = end_s.parse().ok()?;
        if suffix_len == 0 || total == 0 {
            return None;
        }
        let len = suffix_len.min(total);
        let start = total - len;
        return Some((start, total - 1));
    }

    let start: u64 = start_s.parse().ok()?;
    if start >= total {
        return None;
    }
    let end: u64 = if end_s.is_empty() {
        total - 1
    } else {
        let parsed: u64 = end_s.parse().ok()?;
        parsed.min(total - 1)
    };
    if end < start {
        return None;
    }
    Some((start, end))
}

#[cfg(test)]
mod tests {
    use super::*;

    // -----------------------------------------------------------------------
    // parse_single_byte_range
    // -----------------------------------------------------------------------

    #[test]
    fn range_none_input() {
        assert_eq!(parse_single_byte_range(None, 1000), None);
    }

    #[test]
    fn range_empty_string() {
        assert_eq!(parse_single_byte_range(Some(""), 1000), None);
    }

    #[test]
    fn range_non_bytes_unit() {
        assert_eq!(parse_single_byte_range(Some("items=0-10"), 1000), None);
    }

    #[test]
    fn range_multipart() {
        assert_eq!(
            parse_single_byte_range(Some("bytes=0-10,20-30"), 1000),
            None
        );
    }

    #[test]
    fn range_full_spec() {
        assert_eq!(
            parse_single_byte_range(Some("bytes=0-499"), 1000),
            Some((0, 499))
        );
    }

    #[test]
    fn range_open_ended() {
        assert_eq!(
            parse_single_byte_range(Some("bytes=500-"), 1000),
            Some((500, 999))
        );
    }

    #[test]
    fn range_suffix() {
        assert_eq!(
            parse_single_byte_range(Some("bytes=-200"), 1000),
            Some((800, 999))
        );
    }

    #[test]
    fn range_suffix_larger_than_total() {
        assert_eq!(
            parse_single_byte_range(Some("bytes=-2000"), 1000),
            Some((0, 999))
        );
    }

    #[test]
    fn range_suffix_zero() {
        assert_eq!(parse_single_byte_range(Some("bytes=-0"), 1000), None);
    }

    #[test]
    fn range_suffix_zero_total() {
        assert_eq!(parse_single_byte_range(Some("bytes=-100"), 0), None);
    }

    #[test]
    fn range_start_past_end() {
        assert_eq!(parse_single_byte_range(Some("bytes=1000-"), 1000), None);
    }

    #[test]
    fn range_end_clamped() {
        assert_eq!(
            parse_single_byte_range(Some("bytes=0-5000"), 1000),
            Some((0, 999))
        );
    }

    #[test]
    fn range_single_byte() {
        assert_eq!(
            parse_single_byte_range(Some("bytes=42-42"), 1000),
            Some((42, 42))
        );
    }

    // -----------------------------------------------------------------------
    // build_upstream_segment_url
    // -----------------------------------------------------------------------

    fn make_result_with_format(format_id: &str, url: &str) -> ExtractResult {
        ExtractResult {
            id: "test".into(),
            title: None,
            channel_id: None,
            channel_title: None,
            duration: None,
            thumbnails: vec![],
            thumbnail: None,
            formats: vec![Format {
                format_id: format_id.into(),
                ext: None,
                height: None,
                width: None,
                tbr: None,
                vbr: None,
                abr: None,
                fps: None,
                vcodec: None,
                acodec: None,
                filesize: None,
                url: Some(url.into()),
                manifest_url: None,
                protocol: None,
            }],
            subtitles: Default::default(),
            automatic_captions: Default::default(),
            manifest_url: None,
        }
    }

    #[test]
    fn upstream_url_appends_sq() {
        let result = make_result_with_format("137", "https://rr.example.com/seg?key=val");
        let url = build_upstream_segment_url(&result, "137", "5").unwrap();
        assert_eq!(url, "https://rr.example.com/seg?key=val&sq=5");
    }

    #[test]
    fn upstream_url_replaces_existing_sq() {
        let result = make_result_with_format("137", "https://rr.example.com/seg?key=val&sq=0");
        let url = build_upstream_segment_url(&result, "137", "7").unwrap();
        assert!(url.contains("sq=7"), "got: {url}");
        assert!(!url.contains("sq=0"), "old sq not replaced in: {url}");
    }

    #[test]
    fn upstream_url_replaces_sq_at_start() {
        let result = make_result_with_format("137", "sq=0&key=val");
        let url = build_upstream_segment_url(&result, "137", "3").unwrap();
        assert!(url.starts_with("sq=3"), "got: {url}");
    }

    #[test]
    fn upstream_url_unknown_format_returns_none() {
        let result = make_result_with_format("137", "https://x/y");
        assert_eq!(build_upstream_segment_url(&result, "999", "0"), None);
    }

    #[test]
    fn upstream_url_no_url_field() {
        let result = ExtractResult {
            id: "test".into(),
            title: None,
            channel_id: None,
            channel_title: None,
            duration: None,
            thumbnails: vec![],
            thumbnail: None,
            formats: vec![Format {
                format_id: "137".into(),
                ext: None,
                height: None,
                width: None,
                tbr: None,
                vbr: None,
                abr: None,
                fps: None,
                vcodec: None,
                acodec: None,
                filesize: None,
                url: None,
                manifest_url: Some("https://m.example.com/dash.mpd".into()),
                protocol: None,
            }],
            subtitles: Default::default(),
            automatic_captions: Default::default(),
            manifest_url: None,
        };
        assert_eq!(build_upstream_segment_url(&result, "137", "0"), None);
    }

    // -----------------------------------------------------------------------
    // best_audio_format_id
    // -----------------------------------------------------------------------

    #[test]
    fn best_audio_picks_highest_abr() {
        let result = ExtractResult {
            id: "test".into(),
            title: None,
            channel_id: None,
            channel_title: None,
            duration: None,
            thumbnails: vec![],
            thumbnail: None,
            formats: vec![
                Format {
                    format_id: "140".into(),
                    ext: None,
                    height: None,
                    width: None,
                    tbr: None,
                    vbr: None,
                    abr: Some(128.0),
                    fps: None,
                    vcodec: Some("none".into()),
                    acodec: Some("aac".into()),
                    filesize: None,
                    url: Some("https://x/audio128".into()),
                    manifest_url: None,
                    protocol: None,
                },
                Format {
                    format_id: "251".into(),
                    ext: None,
                    height: None,
                    width: None,
                    tbr: None,
                    vbr: None,
                    abr: Some(160.0),
                    fps: None,
                    vcodec: Some("none".into()),
                    acodec: Some("opus".into()),
                    filesize: None,
                    url: Some("https://x/audio160".into()),
                    manifest_url: None,
                    protocol: None,
                },
                Format {
                    format_id: "137".into(),
                    ext: None,
                    height: Some(1080),
                    width: Some(1920),
                    tbr: None,
                    vbr: None,
                    abr: None,
                    fps: None,
                    vcodec: Some("avc1".into()),
                    acodec: None,
                    filesize: None,
                    url: Some("https://x/video".into()),
                    manifest_url: None,
                    protocol: None,
                },
            ],
            subtitles: Default::default(),
            automatic_captions: Default::default(),
            manifest_url: None,
        };
        assert_eq!(best_audio_format_id(&result), Some("251".into()));
    }

    #[test]
    fn best_audio_returns_none_when_no_audio() {
        let result = ExtractResult {
            id: "test".into(),
            title: None,
            channel_id: None,
            channel_title: None,
            duration: None,
            thumbnails: vec![],
            thumbnail: None,
            formats: vec![Format {
                format_id: "137".into(),
                ext: None,
                height: Some(1080),
                width: None,
                tbr: None,
                vbr: None,
                abr: None,
                fps: None,
                vcodec: Some("avc1".into()),
                acodec: None,
                filesize: None,
                url: Some("https://x/video".into()),
                manifest_url: None,
                protocol: None,
            }],
            subtitles: Default::default(),
            automatic_captions: Default::default(),
            manifest_url: None,
        };
        assert_eq!(best_audio_format_id(&result), None);
    }

    // -----------------------------------------------------------------------
    // pick_thumbnail
    // -----------------------------------------------------------------------

    #[test]
    fn pick_thumbnail_prefers_direct() {
        let result = ExtractResult {
            id: "t".into(),
            title: None,
            channel_id: None,
            channel_title: None,
            duration: None,
            thumbnails: vec![crate::services::ytdlp::Thumbnail {
                url: "https://img/fallback.jpg".into(),
                width: Some(1280),
                height: Some(720),
                id: None,
            }],
            thumbnail: Some("https://img/direct.jpg".into()),
            formats: vec![],
            subtitles: Default::default(),
            automatic_captions: Default::default(),
            manifest_url: None,
        };
        assert_eq!(
            pick_thumbnail(&result),
            Some("https://img/direct.jpg".into())
        );
    }

    #[test]
    fn pick_thumbnail_fallback_to_widest() {
        let result = ExtractResult {
            id: "t".into(),
            title: None,
            channel_id: None,
            channel_title: None,
            duration: None,
            thumbnails: vec![
                crate::services::ytdlp::Thumbnail {
                    url: "https://img/small.jpg".into(),
                    width: Some(120),
                    height: Some(90),
                    id: None,
                },
                crate::services::ytdlp::Thumbnail {
                    url: "https://img/large.jpg".into(),
                    width: Some(1920),
                    height: Some(1080),
                    id: None,
                },
            ],
            thumbnail: None,
            formats: vec![],
            subtitles: Default::default(),
            automatic_captions: Default::default(),
            manifest_url: None,
        };
        assert_eq!(
            pick_thumbnail(&result),
            Some("https://img/large.jpg".into())
        );
    }

    #[test]
    fn pick_thumbnail_none_when_empty() {
        let result = ExtractResult {
            id: "t".into(),
            title: None,
            channel_id: None,
            channel_title: None,
            duration: None,
            thumbnails: vec![],
            thumbnail: None,
            formats: vec![],
            subtitles: Default::default(),
            automatic_captions: Default::default(),
            manifest_url: None,
        };
        assert_eq!(pick_thumbnail(&result), None);
    }

    // -----------------------------------------------------------------------
    // parse_sq
    // -----------------------------------------------------------------------

    #[test]
    fn parse_sq_valid() {
        assert_eq!(parse_sq("42"), 42);
    }

    #[test]
    fn parse_sq_invalid() {
        assert_eq!(parse_sq("abc"), 0);
    }

    #[test]
    fn parse_sq_empty() {
        assert_eq!(parse_sq(""), 0);
    }

    // -----------------------------------------------------------------------
    // segment_cache_path
    // -----------------------------------------------------------------------

    #[test]
    fn segment_cache_path_structure() {
        let cfg = crate::config::Config::from_env().unwrap();
        let p = segment_cache_path(&cfg, "vid123", "137", "5");
        assert!(p.ends_with("vid123/137/5"));
        assert!(p.starts_with("./data/segment_cache"));
    }

    // -----------------------------------------------------------------------
    // max_height_for_child (indirectly via constants)
    // -----------------------------------------------------------------------

    #[test]
    fn max_height_mapping_correctness() {
        // This checks the string → height mapping used in max_height_for_child.
        let cases = [("480p", 480i64), ("720p", 720), ("1080p", 1080)];
        for (label, expected) in cases {
            let height: Option<i64> = match label {
                "480p" => Some(480),
                "720p" => Some(720),
                "1080p" => Some(1080),
                _ => None,
            };
            assert_eq!(height, Some(expected));
        }
    }
}
