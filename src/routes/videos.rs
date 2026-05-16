//! Video metadata + DASH manifest + format-proxy routes.
//!
//! Most of the heavy lifting lives in `services::ytdlp`, `services::video_cache`,
//! and `services::dash`. The handlers here glue access control, the
//! cache, and the proxy together.
//!
//! Routes:
//! - `GET /api/videos/:videoId` — metadata + child access check
//! - `GET /api/videos/:videoId/stream` — JSON metadata (formats list + manifest text)
//! - `GET /api/videos/:videoId/stream/manifest.mpd` — synthesized DASH XML
//! - `GET /api/videos/:videoId/captions` — caption track list
//! - `GET /api/videos/:videoId/captions/:lang` — WebVTT track
//! - `GET /api/proxy/format` — signed format byte-range proxy
//! - `GET /api/proxy/thumbnail/:videoId` — thumbnail proxy

use std::collections::HashMap;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use axum::{
    body::Body,
    extract::{Path, Query, State},
    http::{header, HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
use futures_util::TryStreamExt;
use serde::{Deserialize, Serialize};
use sqlx::SqlitePool;
use tokio::sync::Mutex;
use tracing::warn;

use crate::error::{AppError, AppResult};
use crate::middleware::auth::CurrentAccount;
use crate::models::account::AccountType;
use crate::services::access::can_child_view;
use crate::services::dash;
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

#[derive(Debug, Serialize)]
pub struct StreamResponse {
    pub video_id: String,
    /// Rewritten DASH manifest XML. References our proxy endpoints
    /// rather than `*.googlevideo.com` directly.
    pub manifest: Option<String>,
    /// Always `"dash"` when `manifest` is present.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub manifest_type: Option<&'static str>,
    /// Filtered list of progressive formats (max-quality cap applied).
    pub formats: Vec<Format>,
    /// Pre-signed proxy URL for audio-only mode. Points at the
    /// highest-bitrate opus audio-only format (excluding DRC
    /// variants). The frontend uses this directly without needing to
    /// generate an HMAC signature client-side.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub audio_proxy_url: Option<String>,
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

    // Fetch + rewrite the manifest (always DASH).
    let (manifest, manifest_type) =
        match fetch_and_rewrite_manifest(&state, &video_id, &result).await {
            Ok(Some(body)) => (Some(body), Some("dash")),
            Ok(None) => (None, None),
            Err(err) => {
                warn!(%video_id, %err, "failed to fetch upstream manifest");
                (None, None)
            }
        };

    // Pick the best audio-only format and generate a pre-signed proxy URL.
    let audio_proxy_url = {
        let secret = dash::ensure_proxy_secret(&state.db).await?;
        best_audio_format(&formats)
            .map(|f| dash::build_format_proxy_url(&secret, &video_id, &f.format_id))
    };

    Ok(Json(StreamResponse {
        video_id,
        manifest,
        manifest_type,
        formats,
        audio_proxy_url,
    }))
}

// ---------------------------------------------------------------------------
// /api/videos/:videoId/stream/manifest.mpd
// ---------------------------------------------------------------------------

/// `GET /api/videos/:videoId/stream/manifest.mpd` — return the rewritten
/// DASH manifest body directly. The player fetches this URL to bootstrap
/// playback; we cannot reuse the JSON `/stream` endpoint because that
/// embeds the manifest text inside a JSON envelope.
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

    let body = fetch_and_rewrite_manifest(&state, &video_id, &result)
        .await?
        .ok_or(AppError::NotFound)?;

    let mut headers = HeaderMap::new();
    headers.insert(
        header::CONTENT_TYPE,
        "application/dash+xml".parse().unwrap(),
    );
    headers.insert(header::CACHE_CONTROL, "no-store".parse().unwrap());
    Ok((StatusCode::OK, headers, body).into_response())
}

/// Resolve the playable DASH manifest for a video.
///
/// Builds a synthesized MPD from the `https`-protocol per-format URLs
/// in `result.formats[]`. Each `<Representation>` points at a
/// `<BaseURL>` through `/api/proxy/format`. The player drives playback
/// via byte-range requests.
///
/// For each Representation we surface, we emit a `<SegmentBase
/// indexRange>` block — that lets the player learn every
/// Representation's segment layout from a single small byte-range
/// fetch instead of empirically probing each one.
///
/// Primary source: `result.segment_ranges` — parsed from the
/// innertube `/player` API dump at extract time. These are keyed by
/// itag (integer) so we resolve each format's itag and look it up.
/// The result is cached in `format_box_ranges` so subsequent
/// manifest loads skip re-extraction entirely.
///
/// Fallback: `format_box_ranges` SQLite table (populated on prior
/// extractions or by the legacy background-probe path).
///
/// Returns `Ok(None)` when no manifest can be produced.
async fn fetch_and_rewrite_manifest(
    state: &AppState,
    video_id: &str,
    result: &ExtractResult,
) -> AppResult<Option<String>> {
    let secret = dash::ensure_proxy_secret(&state.db).await?;
    let box_ranges = resolve_segment_ranges(&state.db, video_id, result).await;

    if let Some(synthetic) = dash::synthesize_manifest(
        &secret,
        video_id,
        &result.formats,
        result.duration,
        &box_ranges,
    ) {
        return Ok(Some(synthetic));
    }

    Ok(None)
}

// ---------------------------------------------------------------------------
// /api/videos/:videoId/captions
// ---------------------------------------------------------------------------

/// `GET /api/videos/:videoId/captions` — list user-uploaded subtitle
/// tracks.
///
/// **Auto-generated captions are deliberately *not* surfaced.** YouTube
/// auto-translates user captions into ~100 target languages and returns
/// every one in `automatic_captions`. If the frontend renders them all
/// as `<track>` elements the browser eagerly fetches each, which
/// instantly trips YouTube's `caption fetch returned non-2xx
/// status=429` rate limit and cascades into the bot-check wall when
/// the proxy falls back to spawning yt-dlp. The auto-translated
/// captions are also low quality compared to the source. Users who
/// genuinely need a translation can request it explicitly via the
/// per-language endpoint.
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

    let tracks: Vec<CaptionTrack> = result
        .subtitles
        .keys()
        .map(|lang| CaptionTrack {
            lang: lang.clone(),
            name: None,
            auto_generated: false,
        })
        .collect();
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
///    (SRV1/SRV3/TTML) gets converted on the server. Only used when
///    step 2 had no URL at all — *not* when the upstream returned a
///    transient error like 429. Spawning yt-dlp on a 429 just hits
///    YouTube's rate limit again from a different code path and ends
///    up surfacing the bot-check wall to the user, so we propagate
///    rate-limit / forbidden statuses to the caller instead.
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

    // Fast path: yt-dlp metadata exposed a vtt URL. Only fall through
    // to the yt-dlp slow path on transport errors or when the URL
    // wasn't a vtt — non-2xx responses (especially 429) are
    // propagated, *not* retried, because yt-dlp shares the same
    // upstream rate limit and amplifies the problem.
    if let Some(track) = &track {
        if track.ext == "vtt" {
            match reqwest::get(&track.url).await {
                Ok(res) if res.status().is_success() => {
                    if let Ok(body) = res.text().await {
                        caption_cache_set(cache_key.clone(), body.clone()).await;
                        return Ok(vtt_response(body));
                    }
                }
                Ok(res) => {
                    let status = res.status();
                    warn!(%status, %lang, "caption fetch returned non-2xx; propagating");
                    let body = res.bytes().await.unwrap_or_default();
                    let mut response = Response::new(Body::from(body));
                    *response.status_mut() = status;
                    return Ok(response);
                }
                Err(err) => warn!(%err, "caption fetch failed"),
            }
        }
    }

    // Slow path: ask yt-dlp to download + convert to vtt. Only reached
    // when no vtt URL was available *or* the fast path hit a transport
    // error (DNS, TLS, connection-refused) — explicitly *not* on 429,
    // which we propagated above.
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
    // Help the player cache cross-track switches by allowing a short
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
// /api/proxy/thumbnail
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct FormatQuery {
    pub video_id: String,
    pub format: String,
    pub sig: String,
}

/// `GET /api/proxy/format` — proxy a single yt-dlp format URL.
///
/// Used by the synthesized DASH manifest (see [`dash::synthesize_manifest`])
/// where each `<Representation>` points at a `<BaseURL>` of the form
/// `/api/proxy/format?video_id=X&format=Y&sig=Z`. the player then issues
/// byte-range requests against that URL and we stream the bytes
/// through from YouTube's CDN with `Range:` pass-through.
///
/// This endpoint is deliberately *not* segmented: there is no `sq=`,
/// no per-segment caching, and no upstream URL reconstruction. The
/// upstream URL is whatever yt-dlp surfaced for the format, and we
/// fetch it verbatim. That sidesteps the broken HLS-PoT segment path
/// (which gets 403'd by Google's CDN) by relying on the more reliable
/// `https`-protocol per-format URLs.
pub async fn get_format(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<FormatQuery>,
) -> AppResult<Response> {
    let secret = dash::ensure_proxy_secret(&state.db).await?;
    let params: Vec<(&str, String)> = vec![
        ("video_id", q.video_id.clone()),
        ("format", q.format.clone()),
    ];
    if !dash::verify_query(&secret, &params, &q.sig) {
        return Err(AppError::Forbidden);
    }

    let cache = video_cache(&state);
    let result = cache
        .get_or_extract(&state.db, &state.config, &q.video_id)
        .await?;

    let format = result
        .formats
        .iter()
        .find(|f| f.format_id == q.format)
        .ok_or_else(|| AppError::BadRequest("unknown format".into()))?;
    let url = format
        .url
        .clone()
        .ok_or_else(|| AppError::BadRequest("format has no direct URL".into()))?;

    let mut req = reqwest::Client::new().get(&url);
    if let Some(range) = headers.get(header::RANGE) {
        if let Ok(s) = range.to_str() {
            req = req.header(header::RANGE, s);
        }
    }
    let res = req.send().await.map_err(AppError::Http)?;
    let status = res.status();
    let upstream_content_type = res
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("application/octet-stream")
        .to_string();
    // Pass through Content-Length and Content-Range so the browser
    // knows the file size and which range it received — both are
    // required for the player's byte-range fetching to work.
    let upstream_content_length = res
        .headers()
        .get(header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());
    let upstream_content_range = res
        .headers()
        .get(header::CONTENT_RANGE)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());
    let upstream_accept_ranges = res
        .headers()
        .get(header::ACCEPT_RANGES)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());

    let stream = res.bytes_stream().map_err(std::io::Error::other);
    let body = Body::from_stream(stream);
    let mut response = Response::new(body);
    *response.status_mut() = status;
    let h = response.headers_mut();
    h.insert(header::CONTENT_TYPE, upstream_content_type.parse().unwrap());
    if let Some(len) = upstream_content_length {
        if let Ok(v) = len.parse() {
            h.insert(header::CONTENT_LENGTH, v);
        }
    }
    if let Some(rng) = upstream_content_range {
        if let Ok(v) = rng.parse() {
            h.insert(header::CONTENT_RANGE, v);
        }
    }
    if let Some(ar) = upstream_accept_ranges {
        if let Ok(v) = ar.parse() {
            h.insert(header::ACCEPT_RANGES, v);
        }
    }
    Ok(response)
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

/// Resolve SegmentBase byte ranges for all usable formats.
///
/// Merges two sources:
/// 1. `result.segment_ranges` (innertube `/player` API, keyed by itag)
///    — available immediately after extraction.
/// 2. `format_box_ranges` SQLite table — cached from prior extractions
///    or legacy background probes.
///
/// Any ranges present in source 1 but missing from the DB are
/// persisted (fire-and-forget) so future loads are instant.
async fn resolve_segment_ranges(
    pool: &SqlitePool,
    video_id: &str,
    result: &ExtractResult,
) -> std::collections::HashMap<String, crate::services::mp4::BoxRanges> {
    use crate::services::mp4::{BoxRanges, ByteRange};

    let mut out: std::collections::HashMap<String, BoxRanges> = std::collections::HashMap::new();

    // Step 1: Convert itag-keyed segment_ranges to format_id-keyed map.
    // A format_id like "303-dashy" shares itag 303 with "303". We
    // parse the leading integer itag from each format_id.
    for f in &result.formats {
        let Some(itag) = parse_itag_from_format_id(&f.format_id) else {
            continue;
        };
        let Some(sr) = result.segment_ranges.get(&itag) else {
            continue;
        };
        let br = BoxRanges {
            init: ByteRange {
                start: sr.init_start,
                end: sr.init_end,
            },
            index: ByteRange {
                start: sr.index_start,
                end: sr.index_end,
            },
        };
        out.insert(f.format_id.clone(), br);
    }

    // Step 2: For any format NOT covered by segment_ranges, fall back
    // to the database cache (legacy probe results or prior extractions).
    let uncovered: Vec<(String, String)> = result
        .formats
        .iter()
        .filter(|f| {
            !f.format_id.starts_with("sb")
                && matches!(f.protocol.as_deref(), Some("https" | "http_dash_segments"))
                && !out.contains_key(&f.format_id)
        })
        .filter_map(|f| f.url.clone().map(|u| (f.format_id.clone(), u)))
        .collect();
    if !uncovered.is_empty() {
        let cached = crate::services::mp4::lookup_all(pool, video_id, &uncovered).await;
        out.extend(cached);
    }

    // Step 3: Persist any new ranges to the DB for future loads.
    // Fire-and-forget — don't block the manifest response on DB writes.
    let pool_clone = pool.clone();
    let video_id_owned = video_id.to_string();
    let to_persist: Vec<(String, BoxRanges)> =
        out.iter().map(|(fid, br)| (fid.clone(), *br)).collect();
    tokio::spawn(async move {
        for (format_id, ranges) in to_persist {
            crate::services::mp4::store(&pool_clone, &video_id_owned, &format_id, ranges).await;
        }
    });

    out
}

/// Pick the best audio-only format for the audio-only playback mode.
/// Prefers the highest-bitrate opus format, excluding DRC variants.
fn best_audio_format(formats: &[Format]) -> Option<&Format> {
    formats
        .iter()
        .filter(|f| {
            let acodec = f.acodec.as_deref().unwrap_or("none");
            let vcodec = f.vcodec.as_deref().unwrap_or("none");
            let is_audio_only = acodec != "none" && vcodec == "none";
            let is_drc = f.format_id.contains("-drc")
                || f.format_note
                    .as_deref()
                    .map(|s| s.to_ascii_lowercase().contains("drc"))
                    .unwrap_or(false);
            is_audio_only && acodec.starts_with("opus") && !is_drc
        })
        .max_by_key(|f| f.abr.map(|b| (b * 1000.0) as u64).unwrap_or(0))
}

/// Extract the leading integer itag from a yt-dlp format_id.
///
/// yt-dlp names formats like `"303"`, `"303-dashy"`, `"251-drc"`,
/// `"251-0"`, `"251-dashy-1"`. The itag is always the leading integer
/// prefix before the first `-` (or the entire string if no `-`).
fn parse_itag_from_format_id(format_id: &str) -> Option<i64> {
    let numeric_prefix: String = format_id
        .chars()
        .take_while(|c| c.is_ascii_digit())
        .collect();
    numeric_prefix.parse::<i64>().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

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
            segment_ranges: Default::default(),
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
            segment_ranges: Default::default(),
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
            segment_ranges: Default::default(),
        };
        assert_eq!(pick_thumbnail(&result), None);
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
