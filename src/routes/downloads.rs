//! Offline-download routes.
//!
//! Phase 16 of the plan introduces client-side offline downloads. The
//! backend's role is intentionally thin — the actual storage lives in
//! the browser's Cache API (see `frontend/src/services/offline.ts`).
//!
//! The backend just:
//!
//!   1. Tracks each download request in the `offline_downloads` table
//!      (so a parent dashboard can later inspect what kids have saved).
//!   2. Hands the client a single-file stream from the highest matching
//!      progressive format, so the SPA can pipe it straight into the
//!      browser cache without juggling DASH segments.
//!
//! The stream endpoint redirects/proxies to the chosen format URL with
//! range-request support handled by the underlying upstream response.
//!
//! Access control: child accounts must have `child_settings.downloads_enabled = 1`,
//! and the video must pass the regular allowlist check (see
//! [`crate::services::access::can_child_view`]).

use axum::{
    body::Body,
    extract::{Path, Query, State},
    http::{header, HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
use futures_util::TryStreamExt;
use serde::{Deserialize, Serialize};
use sqlx::Row;
use tracing::warn;

use crate::error::{AppError, AppResult};
use crate::middleware::auth::CurrentAccount;
use crate::models::account::AccountType;
use crate::services::access::can_child_view;
use crate::services::video_cache::VideoCache;
use crate::state::AppState;

#[derive(Debug, Deserialize)]
pub struct CreateDownloadBody {
    pub video_id: String,
    pub quality: String,
}

#[derive(Debug, Deserialize)]
pub struct UpdateDownloadBody {
    pub status: Option<String>,
    pub quality: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct DownloadRow {
    pub id: i64,
    pub video_id: String,
    pub video_title: String,
    pub video_thumbnail_url: Option<String>,
    pub channel_title: Option<String>,
    pub quality_label: String,
    pub status: String,
    pub downloaded_at: Option<i64>,
}

fn video_cache(state: &AppState) -> VideoCache {
    static CACHE: std::sync::OnceLock<VideoCache> = std::sync::OnceLock::new();
    let _ = state;
    CACHE.get_or_init(VideoCache::new).clone()
}

async fn ensure_downloads_enabled(state: &AppState, current: &CurrentAccount) -> AppResult<()> {
    // Parents bypass the per-child gate.
    if !matches!(current.account_type, AccountType::Child) {
        return Ok(());
    }
    let enabled: Option<i64> = sqlx::query_scalar(
        "SELECT downloads_enabled FROM child_settings WHERE child_account_id = ?",
    )
    .bind(current.id)
    .fetch_optional(&state.db)
    .await?;
    // Fail-closed: only an explicit `1` opens the gate. A missing row,
    // a `0`, or anything else is treated as disabled. This matches the
    // schema default (`migrations/012_default_downloads_off.sql`) and
    // the UI gate in `routes/pages.rs::fetch_downloads_enabled`.
    if matches!(enabled, Some(1)) {
        Ok(())
    } else {
        Err(AppError::Forbidden)
    }
}

/// `GET /api/downloads` — list the current user's tracked downloads.
pub async fn list(
    State(state): State<AppState>,
    current: CurrentAccount,
) -> AppResult<Json<Vec<DownloadRow>>> {
    // Metadata hydrated from `videos` + `channels` (migrations 024/025).
    let rows = sqlx::query(
        "SELECT od.id, od.video_id, v.title AS video_title, \
                v.thumbnail_url AS video_thumbnail_url, \
                ch.channel_title, od.quality_label, od.status, od.downloaded_at \
         FROM offline_downloads od \
         JOIN videos v ON v.video_id = od.video_id \
         LEFT JOIN channels ch ON ch.channel_id = v.channel_id \
         WHERE od.child_account_id = ? AND od.status != 'deleted' \
         ORDER BY od.id DESC",
    )
    .bind(current.id)
    .fetch_all(&state.db)
    .await?;

    let out = rows
        .into_iter()
        .map(|r| DownloadRow {
            id: r.get("id"),
            video_id: r.get("video_id"),
            video_title: r.get("video_title"),
            video_thumbnail_url: r.get("video_thumbnail_url"),
            channel_title: r.get("channel_title"),
            quality_label: r.get("quality_label"),
            status: r.get("status"),
            downloaded_at: r.get("downloaded_at"),
        })
        .collect();
    Ok(Json(out))
}

/// Reject body- or path-supplied identifiers that aren't shaped like
/// a YouTube video ID. The `stream_url` we build below interpolates
/// `video_id` straight into a URL path segment + query string; without
/// this gate a malicious child could send e.g. `abc?injected=1` in the
/// JSON body and we'd echo it back as
/// `/api/downloads/abc?injected=1/stream?quality=...`, confusing any
/// client (or proxy) that re-parses the path.
///
/// YouTube video IDs are exactly 11 chars from `[A-Za-z0-9_-]`. We
/// match that exactly: downstream code (cache lookups, DB binds, the
/// reverse path in `stream_url`) all assume the canonical YouTube
/// shape, and a wider validator would let `..`/empty IDs slip into
/// filesystem-touching handlers (see `update`/`stream` which take
/// `video_id` from `Path<String>`). If YouTube ever changes the
/// format this validator (and its callers) need re-auditing
/// together; widening "to leave headroom" silently broadens the
/// attack surface in the meantime.
fn is_valid_video_id(id: &str) -> bool {
    let bytes = id.as_bytes();
    bytes.len() == 11
        && bytes
            .iter()
            .all(|b| b.is_ascii_alphanumeric() || *b == b'_' || *b == b'-')
}

/// Reject body-supplied quality labels that aren't shaped like
/// `<digits>p` (e.g. `"720p"`, `"1080p"`) or the literal sentinel
/// `"auto"`. Same defensive rationale as `is_valid_video_id` — the
/// value is echoed into the `stream_url` query string.
///
/// **`"auto"` sentinel:** the frontend (`video-player.ts`) emits
/// `"auto"` when it can't pin a concrete height — i.e. no progressive
/// (muxed audio+video) format is available to derive a `<digits>p`
/// label from. The `stream` handler treats an unparseable quality as
/// "no height cap, pick the best progressive" (see the
/// `trim_end_matches('p').parse()` → `None` path), so `"auto"` is a
/// first-class, load-bearing value — rejecting it breaks downloads of
/// any video lacking a muxed format. It round-trips through the
/// `offline_downloads.quality_label` unique key and the OPFS filename
/// unchanged and is within the unreserved URL alphabet, so it's safe
/// to echo into `stream_url` without encoding.
///
/// **Canonical form is required for the numeric variant**: only
/// `<digits>p` is accepted, not bare `<digits>`. The frontend
/// (`video-player.ts`,
/// `child-settings-form.ts`, `types/index.ts::max_quality`) always
/// emits the `p`-suffixed form, and the downstream consumers
/// (`offline_downloads.quality_label` unique key, yt-dlp `--format`
/// selector, stream-URL formatter) expect the canonical shape. An
/// earlier version of this validator also accepted bare digits "for
/// flexibility," which silently doubled the input alphabet every
/// consumer had to handle without a single caller actually needing
/// it; tightening to one canonical form keeps the contract sharp.
///
/// Three layered checks:
///
/// 1. Shape: 1..=4 ASCII digits followed by a literal `p`. The 1..=4
///    digit range covers the real YouTube label range (`144p` ..=
///    `4320p`); allowing more "for headroom" silently broadens the
///    alphabet downstream consumers accept — same reasoning as
///    `is_valid_video_id`'s `len() == 11` tightening.
///
/// 2. Canonical decimal: reject leading zeros (`"0144p"`, `"0720p"`).
///    These parse fine numerically (`u32::from_str("0144") == 144`)
///    and survive the range check, but they're not the canonical
///    label form the frontend emits — accepting them would silently
///    fork the `quality_label` unique key (a single download could
///    be stored under both `"0720p"` and `"720p"`) and let two
///    distinct strings hit yt-dlp's `--format` selector for the same
///    underlying resolution. The exception is the single literal
///    `"0"`, which is rejected by the range check below regardless.
///
/// 3. Numeric: the parsed leading digits must fall in `144..=4320`.
///    The shape check alone accepts `"0p"`, `"0000p"`, `"0001p"` —
///    all technically URL-safe but nonsensical labels. Parsing and
///    range-checking closes that gap so callers can rely on
///    "validator passes ⇒ recognisable YouTube label."
fn is_valid_quality_label(q: &str) -> bool {
    // `"auto"` is the frontend's "pick the best progressive format"
    // sentinel (emitted when no muxed format yields a concrete height).
    // The `stream` handler already handles it as "no height cap", so
    // accept it as-is alongside the canonical `<digits>p` form.
    if q == "auto" {
        return true;
    }
    let Some(digits) = q.strip_suffix('p') else {
        return false;
    };
    if !(1..=4).contains(&digits.len()) || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return false;
    }
    // Reject leading zeros so the unique key in `offline_downloads`
    // can't be silently forked by callers that emit `"0720p"` and
    // `"720p"` for the same underlying resolution. A single `"0"`
    // would be a valid one-digit string but is filtered out by the
    // range check below anyway.
    if digits.len() > 1 && digits.starts_with('0') {
        return false;
    }
    // Shape passed — parse and range-check. `u32::from_str` can't
    // fail on a 1..=4 all-ASCII-digit string that we already
    // length-checked, but propagate `false` rather than `unwrap` so
    // a future shape-check loosening can't accidentally panic in
    // release.
    match digits.parse::<u32>() {
        Ok(n) => (144..=4320).contains(&n),
        Err(_) => false,
    }
}

/// Reject body-supplied status values that aren't in the
/// `offline_downloads.status` CHECK constraint alphabet.
///
/// The DB-level CHECK provides a hard backstop, but a bad status from
/// the client surfaces there as a sqlx error → 500 — indistinguishable
/// from a transient DB blip. Validate at the route edge so the client
/// gets a clear 400 (matching the `is_valid_video_id` /
/// `is_valid_quality_label` style elsewhere in this file) and the 500
/// channel stays reserved for actual server faults.
///
/// **Keep in sync with `migrations/024_videos_table.sql` line ~355
/// (`CHECK (status IN ('pending', 'downloading', 'complete', 'failed',
/// 'deleted'))`)** — if the migration is ever amended, update this
/// list too or the route will reject values the DB happily accepts.
fn is_valid_download_status(status: &str) -> bool {
    matches!(
        status,
        "pending" | "downloading" | "complete" | "failed" | "deleted"
    )
}

/// `POST /api/downloads` — record a new download request and hand back
/// the stream URL the client should fetch.
pub async fn create(
    State(state): State<AppState>,
    current: CurrentAccount,
    Json(body): Json<CreateDownloadBody>,
) -> AppResult<Json<serde_json::Value>> {
    ensure_downloads_enabled(&state, &current).await?;

    if !is_valid_video_id(&body.video_id) {
        return Err(AppError::BadRequest("invalid video_id".into()));
    }
    if !is_valid_quality_label(&body.quality) {
        return Err(AppError::BadRequest("invalid quality".into()));
    }

    let cache = video_cache(&state);
    let result = cache
        .get_or_extract(&state.db, &state.config, &body.video_id)
        .await?;

    if matches!(current.account_type, AccountType::Child)
        && !can_child_view(
            &state.db,
            current.id,
            &body.video_id,
            result.channel_id.as_deref(),
        )
        .await?
    {
        return Err(AppError::Forbidden);
    }

    let title = result.title.as_deref().filter(|s| !s.is_empty());
    let thumb = result.thumbnail.clone().or_else(|| {
        result
            .thumbnails
            .iter()
            .max_by_key(|t| t.width.unwrap_or(0))
            .map(|t| t.url.clone())
    });

    let mut tx = state.db.begin().await?;
    if let Some(cid) = result.channel_id.as_deref() {
        // Refresh the title for channels we already track, but never
        // CREATE a `channels` row from a direct-video download. An
        // individually-allowlisted video can belong to a channel nobody
        // allowlisted; inserting a row here (with
        // `backfill_next_at`/`rss_next_poll_at` = 0) would enroll that
        // channel in RSS polling + an expensive yt-dlp backfill until GC
        // reaps it. Mirrors the heartbeat path in `routes/usage.rs`;
        // `add_channel`/`add_video` seed the row for tracked channels,
        // so a zero-row UPDATE for an untracked channel is correct.
        sqlx::query(
            "UPDATE channels \
                SET channel_title = COALESCE(NULLIF(?, ''), channel_title) \
              WHERE channel_id = ?",
        )
        .bind(result.channel_title.as_deref())
        .bind(cid)
        .execute(&mut *tx)
        .await?;
    }
    crate::models::video::upsert(
        &mut *tx,
        &body.video_id,
        title,
        result.channel_id.as_deref(),
        result.duration.map(|d| d.round() as i64),
        thumb.as_deref(),
    )
    .await?;
    sqlx::query(
        // Clear `downloaded_at` on the conflict path so a re-queued row
        // (e.g. re-downloading after a soft-delete) doesn't keep its
        // stale completion timestamp while sitting in 'pending'. The
        // `update` handler re-stamps it when the download completes.
        "INSERT INTO offline_downloads (child_account_id, video_id, quality_label, status) \
         VALUES (?, ?, ?, 'pending') \
         ON CONFLICT(child_account_id, video_id, quality_label) \
              DO UPDATE SET status = 'pending', downloaded_at = NULL",
    )
    .bind(current.id)
    .bind(&body.video_id)
    .bind(&body.quality)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;

    // `video_id` and `quality` are gated by `is_valid_video_id` /
    // `is_valid_quality_label` above. After those validators the
    // bytes are restricted to `[A-Za-z0-9_-]{11}` and
    // `\d{1,4}p` — both subsets of the unreserved URL-path/query
    // alphabet (RFC 3986 §2.3), so no percent-encoding is required.
    // If either validator widens its alphabet in the future this
    // formatter must switch to a proper URL encoder.
    let stream_url = format!(
        "/api/downloads/{}/stream?quality={}",
        body.video_id, body.quality
    );
    Ok(Json(serde_json::json!({
        "video_id": body.video_id,
        "quality": body.quality,
        "stream_url": stream_url,
    })))
}

/// `PUT /api/downloads/:videoId` — client status update.
pub async fn update(
    State(state): State<AppState>,
    current: CurrentAccount,
    Path(video_id): Path<String>,
    Json(body): Json<UpdateDownloadBody>,
) -> AppResult<StatusCode> {
    // Re-validate the path-supplied `video_id` against the same
    // alphabet as `create`. `Path<String>` does not constrain the
    // shape — axum just hands us whatever the router matched, which
    // includes `..` and `/`-equivalents that the router pattern
    // happens to accept. The DB bind is safe (parameterised) but a
    // sloppy `video_id` here is a code smell worth catching at the
    // edge.
    if !is_valid_video_id(&video_id) {
        return Err(AppError::BadRequest("invalid video_id".into()));
    }

    let status = body.status.as_deref().unwrap_or("complete");
    if !is_valid_download_status(status) {
        // Validate at the edge so a typo'd status surfaces as a 400
        // rather than the DB CHECK rejection cascading into a 500.
        // The `unwrap_or("complete")` default above is statically
        // safe — `"complete"` is in the allowlist — so an absent
        // body field still takes the happy path.
        return Err(AppError::BadRequest("invalid status".into()));
    }
    let now = chrono::Utc::now().timestamp();
    let downloaded_at: Option<i64> = if status == "complete" {
        Some(now)
    } else {
        None
    };

    // Both branches exclude soft-deleted rows (`status != 'deleted'`)
    // so a `PUT` can't resurrect a download the client already deleted.
    // Re-downloading goes through `create`'s `ON CONFLICT` revive path
    // instead, which is the only intended way to bring a deleted row
    // back to 'pending'.
    //
    // Asymmetric 404 semantics across the two branches:
    //
    // - `quality.is_some()` → the client targets a specific row. If
    //   no live row matches (typo'd quality label, e.g. "1080p" when
    //   only "720p" was recorded, or the row was deleted), silently
    //   204'ing would let the UI think the status flipped. Return 404
    //   so the client can correct.
    //
    // - `quality.is_none()` → the client wants every quality for the
    //   video updated. Zero matching rows is the *expected* outcome
    //   for an idempotent `DELETE`-then-`PUT` sequence (the deleted
    //   rows are now filtered out), so 204 with no row-count gate is
    //   correct.
    //
    // The boolean below makes that asymmetry obvious at the call
    // site; the earlier `Option<u64>` shape was technically correct
    // but read like a check that should fire in both branches.
    let should_404_on_zero_rows = if let Some(quality) = body.quality {
        if !is_valid_quality_label(&quality) {
            return Err(AppError::BadRequest("invalid quality".into()));
        }
        let res = sqlx::query(
            "UPDATE offline_downloads SET status = ?, downloaded_at = ? \
             WHERE child_account_id = ? AND video_id = ? AND quality_label = ? \
               AND status != 'deleted'",
        )
        .bind(status)
        .bind(downloaded_at)
        .bind(current.id)
        .bind(&video_id)
        .bind(&quality)
        .execute(&state.db)
        .await?;
        res.rows_affected() == 0
    } else {
        sqlx::query(
            "UPDATE offline_downloads SET status = ?, downloaded_at = ? \
             WHERE child_account_id = ? AND video_id = ? \
               AND status != 'deleted'",
        )
        .bind(status)
        .bind(downloaded_at)
        .bind(current.id)
        .bind(&video_id)
        .execute(&state.db)
        .await?;
        // Idempotent per the docstring above — never 404 here.
        false
    };
    if should_404_on_zero_rows {
        return Err(AppError::NotFound);
    }
    Ok(StatusCode::NO_CONTENT)
}

/// `DELETE /api/downloads/:videoId` — soft-delete: mark as 'deleted'.
pub async fn delete(
    State(state): State<AppState>,
    current: CurrentAccount,
    Path(video_id): Path<String>,
) -> AppResult<StatusCode> {
    if !is_valid_video_id(&video_id) {
        return Err(AppError::BadRequest("invalid video_id".into()));
    }
    sqlx::query(
        "UPDATE offline_downloads SET status = 'deleted' \
         WHERE child_account_id = ? AND video_id = ?",
    )
    .bind(current.id)
    .bind(&video_id)
    .execute(&state.db)
    .await?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Debug, Deserialize)]
pub struct StreamQuery {
    pub quality: Option<String>,
}

/// `GET /api/downloads/:videoId/stream` — proxy a single-file format
/// suitable for offline playback. Picks the highest progressive
/// (video+audio) format whose height matches the requested quality
/// label or falls below it.
pub async fn stream(
    State(state): State<AppState>,
    current: CurrentAccount,
    Path(video_id): Path<String>,
    headers: HeaderMap,
    Query(q): Query<StreamQuery>,
) -> AppResult<Response> {
    ensure_downloads_enabled(&state, &current).await?;

    // Validate before any cache extraction / yt-dlp invocation. The
    // cache extractor and yt-dlp both treat `video_id` as opaque
    // text, but `..` / shell-metachar / URL-metachar slipping through
    // here would needlessly broaden the attack surface for any
    // downstream consumer that constructs paths/URLs from this value.
    if !is_valid_video_id(&video_id) {
        return Err(AppError::BadRequest("invalid video_id".into()));
    }
    if let Some(quality) = q.quality.as_deref() {
        if !is_valid_quality_label(quality) {
            return Err(AppError::BadRequest("invalid quality".into()));
        }
    }

    let cache = video_cache(&state);
    let result = cache
        .get_or_extract(&state.db, &state.config, &video_id)
        .await?;
    if matches!(current.account_type, AccountType::Child)
        && !can_child_view(
            &state.db,
            current.id,
            &video_id,
            result.channel_id.as_deref(),
        )
        .await?
    {
        return Err(AppError::Forbidden);
    }

    let target_height: Option<i64> = q
        .quality
        .as_deref()
        .and_then(|q| q.trim_end_matches('p').parse::<i64>().ok());
    let mut candidates: Vec<_> = result
        .formats
        .iter()
        .filter(|f| {
            f.url.is_some()
                && f.height.is_some()
                && f.acodec.as_deref() != Some("none")
                && f.vcodec.as_deref() != Some("none")
        })
        .collect();
    if let Some(cap) = target_height {
        candidates.retain(|f| f.height.unwrap_or(0) <= cap);
    }
    candidates.sort_by_key(|f| std::cmp::Reverse(f.height.unwrap_or(0)));
    let chosen = candidates
        .first()
        .copied()
        .ok_or_else(|| AppError::BadRequest("no suitable progressive format".into()))?;
    let url = chosen
        .url
        .clone()
        .ok_or_else(|| AppError::BadRequest("format has no URL".into()))?;

    let mut req = reqwest::Client::new().get(&url);
    if let Some(range) = headers.get(header::RANGE) {
        if let Ok(s) = range.to_str() {
            req = req.header(header::RANGE, s);
        }
    }
    let res = req.send().await.map_err(|err| {
        warn!(%url, %err, "download stream upstream error");
        AppError::Http(err)
    })?;
    let status = res.status();
    let content_type = res
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("video/mp4")
        .to_string();
    let content_length = res.headers().get(header::CONTENT_LENGTH).cloned();

    let stream = res.bytes_stream().map_err(std::io::Error::other);
    let body = Body::from_stream(stream);
    let mut response = (status, body).into_response();
    // Don't unwrap on a remote-supplied header value: the upstream
    // Content-Type passed `to_str().ok()` (so it's UTF-8) but
    // `HeaderValue::from_str` is stricter (rejects e.g. embedded `\0`
    // or some quoted-parameter shapes). Fall back to a safe generic
    // type rather than panicking the whole handler.
    let content_type_header = content_type
        .parse()
        .unwrap_or_else(|_| header::HeaderValue::from_static("application/octet-stream"));
    response
        .headers_mut()
        .insert(header::CONTENT_TYPE, content_type_header);
    if let Some(cl) = content_length {
        response.headers_mut().insert(header::CONTENT_LENGTH, cl);
    }
    response
        .headers_mut()
        .insert(header::ACCEPT_RANGES, "bytes".parse().unwrap());
    Ok(response)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn video_id_accepts_real_youtube_ids() {
        // Real YouTube IDs are exactly 11 chars in [A-Za-z0-9_-].
        assert!(is_valid_video_id("dQw4w9WgXcQ"));
        assert!(is_valid_video_id("abc-def_123"));
        assert!(is_valid_video_id("___---___--"));
    }

    #[test]
    fn video_id_rejects_url_chars_and_wrong_length() {
        // URL-bypass attempts the old comment shrugged off.
        assert!(!is_valid_video_id("abc?injected=1"));
        assert!(!is_valid_video_id("abc/../etc/passwd"));
        assert!(!is_valid_video_id("abc#frag"));
        assert!(!is_valid_video_id("abc def"));
        // Wrong length — anything that isn't exactly 11 chars.
        assert!(!is_valid_video_id(""));
        assert!(!is_valid_video_id("A"));
        assert!(!is_valid_video_id("dQw4w9WgXc")); // 10
        assert!(!is_valid_video_id("dQw4w9WgXcQQ")); // 12
        assert!(!is_valid_video_id(&"a".repeat(17)));
    }

    #[test]
    fn quality_accepts_typical_labels() {
        assert!(is_valid_quality_label("720p"));
        assert!(is_valid_quality_label("1080p"));
        assert!(is_valid_quality_label("144p"));
        assert!(is_valid_quality_label("2160p"));
    }

    #[test]
    fn quality_accepts_auto_sentinel() {
        // `"auto"` is the frontend sentinel for "no muxed format → let
        // the stream handler pick the best progressive". Rejecting it
        // (as an earlier revision of this validator did) breaks
        // downloads of any video lacking a progressive format. The
        // `stream` handler's `trim_end_matches('p').parse()` → `None`
        // path treats it as "no height cap".
        assert!(is_valid_quality_label("auto"));
        // But only the exact literal — no near-misses that could fork
        // the `quality_label` unique key.
        assert!(!is_valid_quality_label("Auto"));
        assert!(!is_valid_quality_label("AUTO"));
        assert!(!is_valid_quality_label("autop"));
        assert!(!is_valid_quality_label("auto "));
        assert!(!is_valid_quality_label("automatic"));
    }

    #[test]
    fn quality_rejects_bare_digits() {
        // Only the canonical `<digits>p` form is accepted; bare
        // digits were a pre-tightening relic that no caller actually
        // emits. See `is_valid_quality_label` docstring.
        assert!(!is_valid_quality_label("720"));
        assert!(!is_valid_quality_label("1080"));
        assert!(!is_valid_quality_label("144"));
        assert!(!is_valid_quality_label("2160"));
        assert!(!is_valid_quality_label("4320"));
    }

    #[test]
    fn quality_rejects_garbage() {
        assert!(!is_valid_quality_label(""));
        assert!(!is_valid_quality_label("p"));
        assert!(!is_valid_quality_label("720p&injected"));
        assert!(!is_valid_quality_label("hd"));
        assert!(!is_valid_quality_label("720 p"));
        assert!(!is_valid_quality_label("720P")); // uppercase 'P' not accepted
                                                  // 5+ digits before the `p`: outside the 144p..=4320p YouTube
                                                  // range. Rejecting "for headroom" silently broadens the
                                                  // alphabet downstream.
        assert!(!is_valid_quality_label("12345p"));
        assert!(!is_valid_quality_label("123456p"));
    }

    #[test]
    fn quality_rejects_out_of_range_shape_passers() {
        // Pass the shape check (1..=4 digits + 'p') but fall outside
        // the real YouTube range (144p..=4320p). The docstring
        // promises we reject these; gate via numeric range not just
        // shape.
        assert!(!is_valid_quality_label("0p"));
        assert!(!is_valid_quality_label("0000p"));
        assert!(!is_valid_quality_label("0001p"));
        assert!(!is_valid_quality_label("143p")); // just below 144
        assert!(!is_valid_quality_label("4321p")); // just above 4320
        assert!(!is_valid_quality_label("9999p"));
    }

    #[test]
    fn quality_accepts_real_youtube_range() {
        // Boundaries of the documented 144p..=4320p YouTube range.
        assert!(is_valid_quality_label("144p"));
        // 4320p (8K) is the largest real YouTube format and must
        // continue to pass the validator.
        assert!(is_valid_quality_label("4320p"));
        // Spot-check common middle values.
        assert!(is_valid_quality_label("240p"));
        assert!(is_valid_quality_label("360p"));
        assert!(is_valid_quality_label("480p"));
        assert!(is_valid_quality_label("720p"));
        assert!(is_valid_quality_label("1080p"));
        assert!(is_valid_quality_label("1440p"));
        assert!(is_valid_quality_label("2160p"));
    }

    #[test]
    fn quality_rejects_leading_zero_decimals() {
        // Canonical-form gate. `u32::from_str("0144") == 144` parses
        // fine and survives the range check, but `"0144p"` is not
        // the canonical label the frontend emits — accepting it
        // would silently fork the `offline_downloads.quality_label`
        // unique key. The single literal `"0"` is rejected by the
        // range check (covered separately) regardless.
        assert!(!is_valid_quality_label("0144p"));
        assert!(!is_valid_quality_label("0720p"));
        assert!(!is_valid_quality_label("01080p"));
        // Sanity: the non-leading-zero canonical forms still pass.
        assert!(is_valid_quality_label("144p"));
        assert!(is_valid_quality_label("720p"));
    }

    #[test]
    fn status_allowlist_matches_db_check() {
        // The exact alphabet of `migrations/024_videos_table.sql`
        // CHECK constraint. If this drifts the route will reject
        // values the DB would happily accept (or vice versa).
        for s in ["pending", "downloading", "complete", "failed", "deleted"] {
            assert!(is_valid_download_status(s), "status {s} should pass");
        }
        for s in [
            "",
            "running",
            "succeeded",
            "queued",
            "Complete",
            "COMPLETE",
            "banana",
        ] {
            assert!(!is_valid_download_status(s), "status {s} should fail");
        }
    }
}
