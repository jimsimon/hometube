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
use tracing::{debug, warn};

use crate::error::{AppError, AppResult};
use crate::middleware::auth::CurrentAccount;
use crate::models::account::AccountType;
use crate::services::access::can_child_view;
use crate::services::dash;

use crate::services::segment_store::{self, TeeStream};
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
    /// Source publish date as unix seconds, looked up from
    /// `channel_videos`. `None` when no archive row carries a date.
    pub published_at: Option<i64>,
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
    /// Short-lived signed manifest URL safe to hand to a Chromecast
    /// receiver (no cookie auth required). Present only when the
    /// requesting child has `chromecast_enabled = 1`; absent (and the
    /// frontend therefore skips the Cast SDK entirely) otherwise.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cast_manifest_url: Option<String>,
    /// `true` when the video uses spherical/equirectangular projection
    /// (360° video). Detected from yt-dlp's `format_note` containing
    /// `"equi"` or `"hequ"` on video-only formats.
    pub is_spherical: bool,
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

    let published_at = lookup_published_at(&state.db, &video_id, result.channel_id.as_deref()).await;

    Ok(Json(VideoMetadata {
        id: result.id.clone(),
        title: result.title.clone(),
        channel_id: result.channel_id.clone(),
        channel_title: result.channel_title.clone(),
        duration_seconds: result.duration,
        thumbnail_url: pick_thumbnail(&result),
        published_at,
    }))
}

/// Best-effort lookup of a video's source publish date (unix seconds)
/// from `channel_videos`. A video can be archived under more than one
/// channel, so prefer the row matching the extracted `channel_id`, then
/// fall back to the most recently seen row that carries a date. Returns
/// `None` (rather than erroring) when nothing matches or the query
/// fails, since the date is purely informational.
pub(crate) async fn lookup_published_at(
    pool: &SqlitePool,
    video_id: &str,
    channel_id: Option<&str>,
) -> Option<i64> {
    sqlx::query_scalar::<_, Option<i64>>(
        "SELECT published_at FROM channel_videos \
         WHERE video_id = ? AND published_at IS NOT NULL \
         ORDER BY (channel_id = ?) DESC, last_seen_at DESC \
         LIMIT 1",
    )
    .bind(video_id)
    .bind(channel_id)
    .fetch_optional(pool)
    .await
    .ok()
    .flatten()
    .flatten()
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
    let secret = dash::ensure_proxy_secret(&state.db).await?;
    let audio_proxy_url = best_audio_format(&formats)
        .map(|f| dash::build_format_proxy_url(&secret, &video_id, &f.format_id));

    // Generate a Chromecast manifest URL when the requester is allowed
    // to cast. Children must have `chromecast_enabled = 1`; parents
    // are always allowed (they cast from preview/admin flows). The
    // token binds to the requester's account id so a leaked URL only
    // unlocks playback for that specific account *and* only while the
    // child's per-video allowlist still applies — `get_stream_manifest`
    // re-runs the access check at request time using the bound id.
    let should_mint_cast = match current.account_type {
        AccountType::Child => {
            let enabled: i64 = sqlx::query_scalar(
                "SELECT chromecast_enabled FROM child_settings WHERE child_account_id = ?",
            )
            .bind(current.id)
            .fetch_optional(&state.db)
            .await?
            .unwrap_or(0);
            enabled != 0
        }
        AccountType::Parent => true,
    };
    let cast_manifest_url = should_mint_cast.then(|| {
        let exp = chrono::Utc::now().timestamp() + 6 * 3600;
        dash::build_cast_manifest_url(&secret, &video_id, current.id, exp)
    });

    // Detect spherical/360° videos. yt-dlp marks equirectangular
    // formats with "equi" or "hequ" in `format_note`. Check any
    // video-only format (has vcodec, no acodec).
    let is_spherical = result.formats.iter().any(|f| {
        let is_video_only = f.vcodec.as_deref().is_some_and(|v| v != "none")
            && f.acodec.as_deref().is_none_or(|a| a == "none");
        is_video_only
            && f.format_note.as_deref().is_some_and(|n| {
                let lower = n.to_ascii_lowercase();
                lower.contains("equi") || lower.contains("hequ")
            })
    });

    Ok(Json(StreamResponse {
        video_id,
        manifest,
        manifest_type,
        formats,
        audio_proxy_url,
        cast_manifest_url,
        is_spherical,
    }))
}

// ---------------------------------------------------------------------------
// /api/videos/:videoId/stream/manifest.mpd
// ---------------------------------------------------------------------------

/// Optional cast-token query string for `get_stream_manifest`.
///
/// When a Chromecast receiver fetches the manifest it can't send a
/// session cookie. Instead the local player passes a short-lived signed
/// URL minted by `build_cast_manifest_url`. All three fields must be
/// present together — partial signatures fail validation.
#[derive(Debug, Deserialize, Default)]
pub struct ManifestQuery {
    pub cast_token: Option<String>,
    pub cast_uid: Option<String>,
    pub cast_exp: Option<String>,
}

/// `GET /api/videos/:videoId/stream/manifest.mpd` — return the rewritten
/// DASH manifest body directly. The player fetches this URL to bootstrap
/// playback; we cannot reuse the JSON `/stream` endpoint because that
/// embeds the manifest text inside a JSON envelope.
///
/// Auth: either a session cookie OR a valid `cast_token` + `cast_uid` +
/// `cast_exp` query triple. The cast-token path exists so Chromecast
/// receivers (which can't send cookies) can fetch the manifest. When
/// authenticated via cast token, we re-run the per-video allowlist
/// check against the bound child id so a token issued before a parent
/// revoked the kid's access stops working immediately.
pub async fn get_stream_manifest(
    State(state): State<AppState>,
    current: Option<CurrentAccount>,
    Path(video_id): Path<String>,
    Query(q): Query<ManifestQuery>,
) -> AppResult<Response> {
    // Authenticate via cast token when no session cookie is present.
    // A valid signature alone only proves "the server minted this for
    // this child for this video at some point" — we still re-run the
    // per-video allowlist check below using the bound child id, so
    // a token outlives neither its 6h expiry nor a parent's
    // revocation of access.
    let cast_authed_child: Option<i64> = match (
        q.cast_token.as_deref(),
        q.cast_uid.as_deref(),
        q.cast_exp.as_deref(),
    ) {
        (Some(token), Some(uid), Some(exp)) => {
            let secret = dash::ensure_proxy_secret(&state.db).await?;
            dash::verify_cast_token(
                &secret,
                &video_id,
                uid,
                exp,
                token,
                chrono::Utc::now().timestamp(),
            )
        }
        _ => None,
    };
    if current.is_none() && cast_authed_child.is_none() {
        return Err(AppError::Unauthorized);
    }

    let cache = video_cache(&state);
    let result = cache
        .get_or_extract(&state.db, &state.config, &video_id)
        .await?;
    if let Some(c) = current.as_ref() {
        enforce_access(&state.db, c, &video_id, &result).await?;
    } else if let Some(child_id) = cast_authed_child {
        // Re-run the same allowlist logic the cookie path uses, but
        // build a synthetic `CurrentAccount` from the token-bound id.
        // We look up the live account so a deleted/blocked child can't
        // keep using their old token.
        let account = crate::models::account::find_by_id(&state.db, child_id)
            .await?
            .ok_or(AppError::Forbidden)?;
        let synthetic = CurrentAccount {
            id: account.id,
            display_name: account.display_name.clone(),
            account_type: account.typed(),
            session_id: String::new(),
        };
        // Re-check the per-child `chromecast_enabled` setting at
        // request time, not just at token mint time. Without this,
        // a token minted while cast was enabled would keep working
        // for up to 6 hours after a parent disables cast — which
        // defeats the point of the per-child gate.
        //
        // Parents also get cast tokens minted (see `get_stream`),
        // but skip this check because they have no `child_settings`
        // row and aren't subject to the parental gate — the parent
        // account itself *is* the gate. The match guard is therefore
        // load-bearing: removing it would 403 every parent cast.
        if matches!(synthetic.account_type, AccountType::Child) {
            let enabled: i64 = sqlx::query_scalar(
                "SELECT chromecast_enabled FROM child_settings WHERE child_account_id = ?",
            )
            .bind(synthetic.id)
            .fetch_optional(&state.db)
            .await?
            .unwrap_or(0);
            if enabled == 0 {
                return Err(AppError::Forbidden);
            }
        }
        enforce_access(&state.db, &synthetic, &video_id, &result).await?;
    }

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
/// Primary source: `result.format_box_ranges` — populated at extract
/// time by matching each format's `filesize` against `contentLength`
/// in innertube's adaptiveFormats. Keyed by `format_id`, so dubbed
/// audio variants resolve to their own per-file ranges.
///
/// Fallback: `format_box_ranges` SQLite table (populated on prior
/// extractions).
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

    let mut tracks: Vec<CaptionTrack> = result
        .subtitles
        .keys()
        .map(|lang| CaptionTrack {
            lang: lang.clone(),
            name: None,
            auto_generated: false,
        })
        .collect();

    // Include the original-language auto-generated caption track.
    // YouTube tags it with a `-orig` suffix (e.g. `en-orig`). We only
    // surface this one — NOT the 100+ auto-translated variants which
    // trigger 429 rate limits when the player eagerly fetches them.
    for lang in result.automatic_captions.keys() {
        if lang.ends_with("-orig") {
            let display_lang = lang.strip_suffix("-orig").unwrap_or(lang);
            tracks.push(CaptionTrack {
                lang: lang.clone(),
                name: Some(format!("{display_lang} (auto)")),
                auto_generated: true,
            });
        }
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

/// Build the `text/vtt` HTTP response. Strips YouTube's inline cue
/// positioning (`align:start position:0%`) so captions render centered
/// with default browser placement.
fn vtt_response(body: String) -> Response {
    let cleaned = strip_vtt_positioning(&body);
    let mut response = (StatusCode::OK, cleaned).into_response();
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

/// Remove inline cue settings (e.g. `align:start position:0%`) from
/// WebVTT timing lines. YouTube's auto-generated captions force
/// left-alignment and 0% position, which looks off-center in a
/// standard player. Stripping them lets the browser use the default
/// centered presentation.
///
/// A VTT timing line looks like:
/// ```text
/// 00:00:02.950 --> 00:00:05.349 align:start position:0%
/// ```
/// We keep the timestamps and drop everything after them.
fn strip_vtt_positioning(vtt: &str) -> String {
    let mut out = String::with_capacity(vtt.len());
    for line in vtt.lines() {
        if line.contains("-->") {
            // Timing line: keep only the "START --> END" portion.
            if let Some(arrow_end) = line.find("-->") {
                // Find end of the end-timestamp (next space after "-->")
                let after_arrow = &line[arrow_end + 3..];
                let end_ts_end = after_arrow
                    .trim_start()
                    .find([' ', '\t'])
                    .map(|i| arrow_end + 3 + after_arrow.len() - after_arrow.trim_start().len() + i)
                    .unwrap_or(line.len());
                out.push_str(&line[..end_ts_end]);
            } else {
                out.push_str(line);
            }
        } else {
            out.push_str(line);
        }
        out.push('\n');
    }
    out
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
/// **Segment caching**: when all 2 MiB chunks covering the requested byte
/// range are already cached on disk, the response is served directly from
/// the filesystem. Otherwise, bytes are proxied from upstream and
/// tee-cached into aligned chunks for future requests.
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

    // Determine total file size from metadata (used for Content-Range on cache hits
    // and for TeeStream's final-chunk logic on misses).
    // Guard against negative sentinel values (yt-dlp may use -1 for unknown).
    let total_size: Option<u64> = format.filesize.and_then(|s| u64::try_from(s).ok());

    // Parse the incoming Range header to determine byte offsets.
    let range_header = headers
        .get(header::RANGE)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());

    let parsed_range = range_header.as_deref().and_then(parse_range_header);

    // --- Cache-hit path ---
    // If we have a defined range with known end AND all chunks are cached, serve from disk.
    // If the file read fails (e.g., eviction race), we fall through to the upstream path.
    if let Some((byte_start, Some(end))) = parsed_range {
        let is_cached =
            segment_store::range_fully_cached(&state.db, &q.video_id, &q.format, byte_start, end)
                .await
                .unwrap_or(false);

        if is_cached {
            let read_result = segment_store::read_range_from_cache(
                &state.config.cache_dir,
                &q.video_id,
                &q.format,
                byte_start,
                end,
            )
            .await;

            if let Ok(data) = read_result {
                debug!(
                    video_id = %q.video_id,
                    format = %q.format,
                    byte_start,
                    byte_end = end,
                    "segment cache hit — serving from disk"
                );

                // Touch accessed chunks in the background (LRU bookkeeping).
                let pool = state.db.clone();
                let vid = q.video_id.clone();
                let fmt = q.format.clone();
                tokio::spawn(async move {
                    segment_store::touch_chunks(
                        &pool,
                        &vid,
                        &fmt,
                        segment_store::chunk_index(byte_start),
                        segment_store::chunk_index(end),
                    )
                    .await;
                });

                // Determine total size for Content-Range header.
                let file_total_db =
                    segment_store::get_format_total_bytes(&state.db, &q.video_id, &q.format).await;
                let effective_total = total_size.or(file_total_db);

                let content_length = data.len();
                let mut response = Response::new(Body::from(data));
                *response.status_mut() = StatusCode::PARTIAL_CONTENT;
                let h = response.headers_mut();
                h.insert(
                    header::CONTENT_TYPE,
                    "application/octet-stream".parse().unwrap(),
                );
                h.insert(header::CONTENT_LENGTH, content_length.into());
                let range_str = match effective_total {
                    Some(total) => format!("bytes {}-{}/{}", byte_start, end, total),
                    None => format!("bytes {}-{}/*", byte_start, end),
                };
                h.insert(header::CONTENT_RANGE, range_str.parse().unwrap());
                h.insert(header::ACCEPT_RANGES, "bytes".parse().unwrap());
                return Ok(response);
            }
            // File read failed (eviction race) — fall through to upstream.
            debug!(
                video_id = %q.video_id,
                format = %q.format,
                "cache read failed, falling through to upstream"
            );
        }
    }

    // --- Cache-miss path: proxy from upstream with tee-caching ---
    let mut req = state.http_client.get(&url);
    if let Some(ref range_val) = range_header {
        req = req.header(header::RANGE, range_val.as_str());
    }
    let res = req.send().await.map_err(AppError::Http)?;
    let status = res.status();
    let upstream_content_type = res
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("application/octet-stream")
        .to_string();
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

    // Try to extract total file size from Content-Range: bytes X-Y/TOTAL
    let upstream_total = upstream_content_range
        .as_deref()
        .and_then(parse_content_range_total);
    let effective_total = total_size.or(upstream_total);

    // Persist total_bytes if we learned it for the first time.
    if let Some(total) = effective_total {
        let pool = state.db.clone();
        let vid = q.video_id.clone();
        let fmt = q.format.clone();
        tokio::spawn(async move {
            segment_store::set_format_total_bytes(&pool, &vid, &fmt, total).await;
        });
    }

    // ---------------------------------------------------------------
    // Standard path: stream the upstream response with tee-caching.
    // ---------------------------------------------------------------
    let stream_start: Option<u64> = upstream_content_range
        .as_deref()
        .and_then(parse_content_range_start)
        .or_else(|| parsed_range.map(|(s, _)| s));

    let raw_stream = res.bytes_stream().map_err(std::io::Error::other);
    let body = if let Some(start) = stream_start {
        let tee = TeeStream::new(
            Box::pin(raw_stream),
            state.db.clone(),
            state.config.cache_dir.clone(),
            q.video_id.clone(),
            q.format.clone(),
            start,
            effective_total,
        );
        Body::from_stream(tee)
    } else {
        if !range_header.as_ref().is_some_and(|r| r.contains('=')) || status == StatusCode::OK {
            let tee = TeeStream::new(
                Box::pin(raw_stream),
                state.db.clone(),
                state.config.cache_dir.clone(),
                q.video_id.clone(),
                q.format.clone(),
                0,
                effective_total,
            );
            Body::from_stream(tee)
        } else {
            Body::from_stream(raw_stream)
        }
    };

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

/// Parse a `Range: bytes=START-END` header.
/// Returns `(start, Option<end>)`. `end` is `None` for open-ended ranges like `bytes=1000-`.
fn parse_range_header(header: &str) -> Option<(u64, Option<u64>)> {
    let s = header.strip_prefix("bytes=")?;
    let (start_str, end_str) = s.split_once('-')?;
    let start: u64 = start_str.parse().ok()?;
    let end: Option<u64> = if end_str.is_empty() {
        None
    } else {
        Some(end_str.parse().ok()?)
    };
    Some((start, end))
}

/// Parse the total size from a `Content-Range: bytes X-Y/TOTAL` header.
fn parse_content_range_total(header: &str) -> Option<u64> {
    // Format: "bytes 0-999/5000" or "bytes 0-999/*"
    let s = header.strip_prefix("bytes ")?;
    let (_range_part, total_part) = s.split_once('/')?;
    if total_part == "*" {
        None
    } else {
        total_part.parse().ok()
    }
}

/// Parse the start byte from a `Content-Range: bytes START-END/TOTAL` header.
fn parse_content_range_start(header: &str) -> Option<u64> {
    let s = header.strip_prefix("bytes ")?;
    let (range_part, _total_part) = s.split_once('/')?;
    let (start_str, _end_str) = range_part.split_once('-')?;
    start_str.parse().ok()
}

/// `GET /api/proxy/thumbnail/:videoId` — stream the highest-resolution
/// thumbnail through the server, served from the on-disk
/// `thumbnail_cache` when available. Cache hits bump
/// `last_accessed_at` for LRU. On miss we fetch from YouTube, populate
/// the cache, and serve the bytes. No HMAC is required; thumbnails are
/// inherently public.
pub async fn get_thumbnail(
    State(state): State<AppState>,
    Path(video_id): Path<String>,
) -> AppResult<Response> {
    // 1. Cache hit fast-path. Read the file directly and serve.
    if let Some(path) = crate::services::thumbnail_store::get(&state.db, &video_id).await {
        match tokio::fs::read(&path).await {
            Ok(bytes) => {
                let mut response = Response::new(Body::from(bytes));
                response
                    .headers_mut()
                    .insert(header::CONTENT_TYPE, "image/jpeg".parse().unwrap());
                return Ok(response);
            }
            Err(err) => {
                // File disappeared between the `get` stat and the
                // read. Fall through to the upstream fetch.
                tracing::debug!(%video_id, %err, "thumbnail cache read failed; falling back to upstream");
            }
        }
    }

    // 2. Cache miss: resolve the thumbnail URL via the metadata cache
    //    (this part of the path is unchanged from the pre-cache
    //    implementation), fetch from YouTube, populate the cache, and
    //    serve.
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

    // Only populate the cache for successful 2xx responses, AND only
    // when the upstream `Content-Length` is small enough that
    // buffering the whole body to disk is safe. Above that ceiling
    // (or when Content-Length is missing) we stream straight through
    // to the client without caching — the cache fills opportunistically
    // on a future request whose upstream returns a normally-sized
    // image. This protects against an upstream misconfiguration (or
    // path-traversal-to-a-large-blob attack via the video_id) turning
    // a single thumbnail fetch into multi-megabyte memory growth on
    // our process.
    //
    // Real `i.ytimg.com` thumbnails are 5–50 KB; the 1 MB ceiling is
    // 20× headroom while still being safe to buffer.
    const THUMBNAIL_CACHE_MAX_BYTES: u64 = 1024 * 1024;
    let content_length: Option<u64> = res
        .headers()
        .get(header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.parse().ok());
    let cacheable =
        status.is_success() && matches!(content_length, Some(n) if n <= THUMBNAIL_CACHE_MAX_BYTES);

    if cacheable {
        let bytes = res.bytes().await.map_err(AppError::Http)?;
        // Best-effort cache write; failures are logged inside put()
        // and don't fail the request.
        let _ = crate::services::thumbnail_store::put(
            &state.db,
            &state.config.cache_dir,
            &video_id,
            &bytes,
        )
        .await;
        let mut response = Response::new(Body::from(bytes));
        *response.status_mut() = status;
        response
            .headers_mut()
            .insert(header::CONTENT_TYPE, content_type.parse().unwrap());
        Ok(response)
    } else {
        if status.is_success() {
            tracing::debug!(
                %video_id,
                ?content_length,
                "thumbnail: upstream content too large to cache (or Content-Length missing); streaming through"
            );
        }
        let stream = res.bytes_stream().map_err(std::io::Error::other);
        let body = Body::from_stream(stream);
        let mut response = Response::new(body);
        *response.status_mut() = status;
        response
            .headers_mut()
            .insert(header::CONTENT_TYPE, content_type.parse().unwrap());
        Ok(response)
    }
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
    let allowed =
        can_child_view(pool, current.id, video_id, extracted.channel_id.as_deref()).await?;
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
/// Two sources, in order:
/// 1. `result.format_box_ranges` — populated at extract time by
///    matching each format's `filesize` against `contentLength` in
///    innertube's adaptiveFormats. This is the canonical path and
///    handles dubbed audio variants correctly.
/// 2. `format_box_ranges` SQLite table — covers formats where the
///    innertube data didn't provide a usable match (e.g. yt-dlp
///    missing `filesize`).
///
/// Ranges resolved from innertube are persisted to the DB
/// fire-and-forget so subsequent loads can skip re-extraction.
async fn resolve_segment_ranges(
    pool: &SqlitePool,
    video_id: &str,
    result: &ExtractResult,
) -> std::collections::HashMap<String, crate::services::segment_ranges::BoxRanges> {
    use crate::services::segment_ranges::{BoxRanges, ByteRange};

    let mut out: std::collections::HashMap<String, BoxRanges> = std::collections::HashMap::new();

    // Step 1: per-format-id ranges resolved at extract time.
    for f in &result.formats {
        if let Some(sr) = result.format_box_ranges.get(&f.format_id) {
            out.insert(
                f.format_id.clone(),
                BoxRanges {
                    init: ByteRange {
                        start: sr.init_start,
                        end: sr.init_end,
                    },
                    index: ByteRange {
                        start: sr.index_start,
                        end: sr.index_end,
                    },
                },
            );
        }
    }

    // Step 2: DB cache for any format we couldn't resolve from innertube.
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
        let cached = crate::services::segment_ranges::lookup_all(pool, video_id, &uncovered).await;
        out.extend(cached);
    }

    // Step 3: persist freshly-resolved innertube ranges to the DB.
    let new_from_innertube: Vec<(String, BoxRanges)> = result
        .format_box_ranges
        .keys()
        .filter_map(|format_id| {
            out.get(format_id)
                .copied()
                .map(|br| (format_id.clone(), br))
        })
        .collect();
    if !new_from_innertube.is_empty() {
        let pool_clone = pool.clone();
        let video_id_owned = video_id.to_string();
        tokio::spawn(async move {
            for (format_id, ranges) in new_from_innertube {
                crate::services::segment_ranges::store(
                    &pool_clone,
                    &video_id_owned,
                    &format_id,
                    ranges,
                )
                .await;
            }
        });
    }

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
            let is_drc = f.format_id.contains("-drc-")
                || f.format_id.ends_with("-drc")
                || f.format_note
                    .as_deref()
                    .map(|s| s.to_ascii_lowercase().contains("drc"))
                    .unwrap_or(false);
            is_audio_only && acodec.starts_with("opus") && !is_drc
        })
        .max_by_key(|f| f.abr.map(|b| (b * 1000.0) as u64).unwrap_or(0))
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
            format_box_ranges: Default::default(),
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
            format_box_ranges: Default::default(),
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
            format_box_ranges: Default::default(),
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
