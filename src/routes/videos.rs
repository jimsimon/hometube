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
    /// Rewritten DASH manifest text. The XML references segment URLs
    /// pointing at our `/api/proxy/segment` endpoint.
    pub manifest: Option<String>,
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

    // If there's a DASH manifest URL on the result, fetch + rewrite it.
    let manifest_url = result
        .manifest_url
        .clone()
        .or_else(|| result.formats.iter().find_map(|f| f.manifest_url.clone()));
    let manifest = match manifest_url {
        Some(url) => {
            let secret = dash::ensure_proxy_secret(&state.db).await?;
            match reqwest::get(&url).await {
                Ok(res) if res.status().is_success() => {
                    let body = res.text().await.unwrap_or_default();
                    Some(dash::rewrite_manifest(&secret, &video_id, &body)?)
                }
                Ok(res) => {
                    warn!(%url, status = %res.status(), "failed to fetch DASH manifest");
                    None
                }
                Err(err) => {
                    warn!(%url, %err, "failed to fetch DASH manifest");
                    None
                }
            }
        }
        None => None,
    };

    Ok(Json(StreamResponse {
        video_id,
        manifest,
        formats,
    }))
}

// ---------------------------------------------------------------------------
// /api/videos/:videoId/stream/manifest.mpd
// ---------------------------------------------------------------------------

/// `GET /api/videos/:videoId/stream/manifest.mpd` — return the rewritten
/// DASH XML body directly. vidstack's dash.js provider fetches this URL
/// to bootstrap playback; we cannot reuse the JSON `/stream` endpoint
/// because it returns the manifest as a string field inside JSON.
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

    let manifest_url = result
        .manifest_url
        .clone()
        .or_else(|| result.formats.iter().find_map(|f| f.manifest_url.clone()));
    let url = manifest_url.ok_or(AppError::NotFound)?;

    let secret = dash::ensure_proxy_secret(&state.db).await?;
    let res = reqwest::get(&url).await?;
    if !res.status().is_success() {
        warn!(%url, status = %res.status(), "non-2xx fetching DASH manifest");
        return Err(AppError::NotFound);
    }
    let body = res.text().await?;
    let rewritten = dash::rewrite_manifest(&secret, &video_id, &body)?;

    let mut headers = HeaderMap::new();
    headers.insert(
        header::CONTENT_TYPE,
        "application/dash+xml".parse().unwrap(),
    );
    headers.insert(header::CACHE_CONTROL, "no-store".parse().unwrap());
    Ok((StatusCode::OK, headers, rewritten).into_response())
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
fn parse_single_byte_range(value: Option<&str>, total: u64) -> Option<(u64, u64)> {
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
