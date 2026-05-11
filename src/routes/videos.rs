//! Video metadata + DASH manifest + segment-proxy routes.
//!
//! Most of the heavy lifting lives in `services::ytdlp`, `services::video_cache`,
//! and `services::dash`. The handlers here glue access control, the
//! cache, and the proxy together.
//!
//! Routes:
//! - `GET /api/videos/:videoId` — metadata + child access check
//! - `GET /api/videos/:videoId/stream` — DASH manifest + filtered formats
//! - `GET /api/videos/:videoId/captions` — caption track list
//! - `GET /api/videos/:videoId/captions/:lang` — WebVTT track
//! - `GET /api/proxy/segment` — signed DASH segment proxy
//! - `GET /api/proxy/audio` — audio-only stream proxy
//! - `GET /api/proxy/thumbnail/:videoId` — thumbnail proxy

use std::path::PathBuf;

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
        .or_else(|| {
            result
                .formats
                .iter()
                .find_map(|f| f.manifest_url.clone())
        });
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

/// `GET /api/videos/:videoId/captions/:lang` — fetch the source caption
/// file from YouTube and convert to WebVTT. yt-dlp returns a URL per
/// language; we pick the WebVTT/ttml/vtt variant where possible and
/// stream it through.
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
            result
                .automatic_captions
                .get(&lang)
                .and_then(|tracks| {
                    tracks
                        .iter()
                        .find(|t| t.ext == "vtt")
                        .or_else(|| tracks.first())
                })
        })
        .ok_or(AppError::NotFound)?;

    let res = reqwest::get(&track.url).await.map_err(AppError::Http)?;
    if !res.status().is_success() {
        return Err(AppError::Other(anyhow::anyhow!(
            "caption fetch returned {}",
            res.status()
        )));
    }
    let body = res.text().await.map_err(AppError::Http)?;
    // If yt-dlp gave us a non-VTT format we'd convert here. For Phase 5
    // we ship the bytes through as-is and rely on yt-dlp's vtt
    // preference; non-vtt formats are rare for YouTube.
    let mut response = (StatusCode::OK, body).into_response();
    response
        .headers_mut()
        .insert(header::CONTENT_TYPE, "text/vtt; charset=utf-8".parse().unwrap());
    Ok(response)
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
        crate::services::cron::CACHE_HIT_COUNTER
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        return serve_file(&path, size, &headers).await;
    }
    crate::services::cron::CACHE_MISS_COUNTER
        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);

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
    pub sig: String,
}

/// `GET /api/proxy/audio` — proxy a single audio-only stream URL.
/// Audio formats expose a stable `url` (no segment numbers); the
/// signature is over `(video_id, format)`.
pub async fn get_audio(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<AudioQuery>,
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
        .as_ref()
        .ok_or_else(|| AppError::BadRequest("format has no direct URL".into()))?;

    let mut req = reqwest::Client::new().get(url);
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
    let stream = res.bytes_stream().map_err(std::io::Error::other);
    let body = Body::from_stream(stream);
    let mut response = Response::new(body);
    *response.status_mut() = status;
    response
        .headers_mut()
        .insert(header::CONTENT_TYPE, content_type.parse().unwrap());
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
    let allowed = can_child_view(
        pool,
        current.id,
        video_id,
        extracted.channel_id.as_deref(),
        &[],
    )
    .await?;
    if !allowed {
        return Err(AppError::Forbidden);
    }
    Ok(())
}

async fn max_height_for_child(pool: &SqlitePool, child_id: i64) -> AppResult<Option<i64>> {
    let row: Option<(Option<String>,)> = sqlx::query_as(
        "SELECT max_quality FROM child_settings WHERE child_account_id = ?",
    )
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

fn build_upstream_segment_url(
    result: &ExtractResult,
    format_id: &str,
    sq: &str,
) -> Option<String> {
    let format = result
        .formats
        .iter()
        .find(|f| f.format_id == format_id)?;

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

async fn write_segment(path: &PathBuf, bytes: &[u8]) -> std::io::Result<()> {
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
    path: &PathBuf,
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

async fn serve_file(
    path: &PathBuf,
    _size: i64,
    _headers: &HeaderMap,
) -> AppResult<Response> {
    // TODO(phase-12): honour the `Range` header on cache hits so the
    // player can seek inside an already-cached segment without a full
    // re-download. Segments are typically only 2-5 MB so the
    // performance impact of always serving the whole file is small.
    let bytes = tokio::fs::read(path)
        .await
        .map_err(|e| AppError::Other(anyhow::anyhow!("reading cached segment: {e}")))?;
    let body = Body::from(bytes);
    let mut response = Response::new(body);
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        "application/octet-stream".parse().unwrap(),
    );
    Ok(response)
}
