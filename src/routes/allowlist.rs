//! Allowlist management routes (parent only).
//!
//! Two flavours: channels and individual videos. Each follows the same
//! shape:
//!
//! - `GET    /api/children/:id/allowlist/{kind}`
//! - `POST   /api/children/:id/allowlist/{kind}`           (body: `{ channel_id|video_id }`)
//! - `DELETE /api/children/:id/allowlist/{kind}/:itemId`
//!
//! The `:id` path parameter must refer to a *child* account; parent IDs
//! are rejected with `400 Bad Request`. Metadata (title, thumbnail) is
//! fetched from YouTube (via the discovery sidecar) at insert time so
//! the UI doesn't have to re-resolve names every time it lists the
//! allowlist.

use axum::{
    extract::{Path, State},
    http::StatusCode,
    Json,
};
use serde::{Deserialize, Serialize};

use crate::error::{AppError, AppResult};
use crate::middleware::auth::CurrentAccount;
use crate::services::access;
use crate::services::youtube::YoutubeClient;
use crate::state::AppState;

// ---------------------------------------------------------------------------
// Channels
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize, sqlx::FromRow)]
pub struct AllowlistedChannel {
    pub id: i64,
    pub channel_id: String,
    pub channel_title: String,
    pub channel_thumbnail_url: Option<String>,
    pub created_at: i64,
}

/// Body for `POST /api/children/:id/allowlist/channels`.
///
/// `channel_id` is required; the rest are caller-supplied metadata
/// from the parent search response that the server uses **in
/// preference to** calling the discovery sidecar. The dominant
/// allowlist flow is "parent searches → clicks a result → adds":
/// the search response already contains the title and thumbnail, so
/// forwarding them in the POST body lets the server skip the sidecar
/// `/channels/:id` call entirely — eliminating an anti-bot-sensitive
/// burst surface when many channels are added in quick succession.
///
/// Body data wins when present; the sidecar is only called as a
/// fallback when `channel_title` is missing (e.g. raw URL/ID pastes).
/// This mirrors the existing `AddVideoBody` pattern but with a
/// stronger preference for body data.
#[derive(Debug, Deserialize)]
pub struct AddChannelBody {
    pub channel_id: String,
    #[serde(default)]
    pub channel_title: Option<String>,
    #[serde(default)]
    pub channel_thumbnail_url: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
}

/// `GET /api/children/:id/allowlist/channels`.
pub async fn list_channels(
    State(state): State<AppState>,
    Path(child_id): Path<i64>,
) -> AppResult<Json<Vec<AllowlistedChannel>>> {
    require_child_id(&state, child_id).await?;
    // Channel metadata now lives on `channels` (migration 025); the
    // `allowlisted_channels` row is just a FK linkage.
    let rows: Vec<AllowlistedChannel> = sqlx::query_as(
        "SELECT ac.id, ac.channel_id, \
                COALESCE(ch.channel_title, '') AS channel_title, \
                ch.channel_thumbnail_url, ac.created_at \
         FROM allowlisted_channels ac \
         LEFT JOIN channels ch ON ch.channel_id = ac.channel_id \
         WHERE ac.child_account_id = ? \
         ORDER BY ac.created_at DESC",
    )
    .bind(child_id)
    .fetch_all(&state.db)
    .await?;
    Ok(Json(rows))
}

/// Maximum length for the body-supplied `channel_title`. YouTube
/// caps channel names at 100 chars; we accept up to 256 to leave room
/// for unicode normalisation differences.
const MAX_CHANNEL_TITLE_LEN: usize = 256;
/// Maximum length for the body-supplied `channel_id`. Real YouTube
/// IDs are 24 chars (`UC` + 22 base64url chars); 64 is comfortable.
const MAX_CHANNEL_ID_LEN: usize = 64;
/// Maximum length for the body-supplied thumbnail URL. The longest
/// legitimate `i.ytimg.com` thumbnail URL is well under 200 chars.
/// Reject anything past 2 KiB to prevent payload bloat in
/// `allowlisted_channels.channel_thumbnail_url`.
const MAX_THUMBNAIL_URL_LEN: usize = 2048;
/// Maximum length for the body-supplied `description`. YouTube caps
/// channel descriptions at 5,000 chars; we accept up to 8,192 for
/// unicode headroom.
const MAX_DESCRIPTION_LEN: usize = 8192;

/// Validate that a body-supplied thumbnail URL is `https://` and
/// (loosely) a YouTube-controlled host. Body data flows from the
/// parent search response which is generated server-side from a
/// sidecar call to `youtubei.js` — legitimate URLs are always under
/// `*.ytimg.com` or `*.googleusercontent.com`. Rejecting anything
/// else protects child UIs from rendering an attacker-controlled
/// host via `<img src>` if a malicious parent (or compromised parent
/// session) injects a body-data forgery.
fn is_safe_thumbnail_url(url: &str) -> bool {
    let url = url.trim();
    if url.is_empty() || url.len() > MAX_THUMBNAIL_URL_LEN {
        return false;
    }
    let Some(rest) = url.strip_prefix("https://") else {
        return false;
    };

    // The authority component of a URL ends at the FIRST occurrence
    // of `/`, `?`, or `#` (per RFC 3986). Splitting on `/` alone
    // would have let an attacker hide the real host behind a query
    // or fragment delimiter:
    //
    //     https://attacker.com#@x.ytimg.com/foo
    //
    // `rest.split('/').next()` returns
    // `"attacker.com#@x.ytimg.com"`; the subsequent `@`-split picks
    // `x.ytimg.com` as the apparent host, the validator returns
    // true, and the browser then fetches `attacker.com` (the
    // fragment is client-side only). The same bypass applies with
    // `?` in place of `#`. Splitting on all three terminators
    // closes both holes.
    //
    // We resist reaching for the `url` crate here for the same
    // reason this helper exists — the `url` crate's `Url::parse`
    // would happily accept the malicious URL too; we'd still have
    // to check `.host_str()` against our suffix list afterward, and
    // ad-hoc-parse the userinfo. The set of corner cases relevant
    // to thumbnail validation is small enough that an inline parser
    // is honest about what it's doing.
    let host = rest.split(['/', '?', '#']).next().unwrap_or("");

    // Reject any userinfo. The `@`-split approach we previously
    // used resolved the host to the right-of-`@` segment (which
    // works for legitimate `user:pass@host/...` URLs the browser
    // would fetch correctly), but accepting userinfo at all
    // creates a confusing wart for a validator whose job is to
    // gate user-visible <img src> URLs. There's no legitimate
    // reason for a thumbnail URL to carry HTTP basic-auth
    // credentials; reject outright.
    if host.contains('@') {
        return false;
    }

    // Strip an optional port (`host:443`). IPv6 literals carry
    // brackets (`[::1]:443`) which we reject implicitly via the
    // suffix check below.
    let host = host.split(':').next().unwrap_or(host);
    let host_lower = host.to_ascii_lowercase();
    // Accept hosts that match a known YouTube/Google CDN suffix. The
    // `==` arms cover the (rare) bare-domain case; the `ends_with`
    // arms must include the leading `.` so a suffix-spoof like
    // `ytimg.com.evil` is rejected.
    host_lower.ends_with(".ytimg.com")
        || host_lower == "ytimg.com"
        || host_lower.ends_with(".ggpht.com")
        || host_lower == "ggpht.com"
        || host_lower.ends_with(".googleusercontent.com")
        || host_lower == "googleusercontent.com"
}

/// `POST /api/children/:id/allowlist/channels`.
pub async fn add_channel(
    State(state): State<AppState>,
    current: CurrentAccount,
    Path(child_id): Path<i64>,
    Json(body): Json<AddChannelBody>,
) -> AppResult<Json<AllowlistedChannel>> {
    require_child_id(&state, child_id).await?;

    // Reject obviously-bad channel_id early — keeps us from
    // round-tripping unbounded blobs through the rest of the handler
    // and the sidecar.
    let body_channel_id = body.channel_id.trim();
    if body_channel_id.is_empty() {
        return Err(AppError::BadRequest("channel_id required".into()));
    }
    if body_channel_id.len() > MAX_CHANNEL_ID_LEN {
        return Err(AppError::BadRequest(format!(
            "channel_id too long (max {MAX_CHANNEL_ID_LEN} chars)"
        )));
    }

    // 1. Try body data first (trim + filter empty per add_video convention).
    let body_title = body
        .channel_title
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty());
    if let Some(t) = body_title {
        if t.chars().count() > MAX_CHANNEL_TITLE_LEN {
            return Err(AppError::BadRequest(format!(
                "channel_title too long (max {MAX_CHANNEL_TITLE_LEN} chars)"
            )));
        }
    }
    let body_thumb = body
        .channel_thumbnail_url
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty());
    if let Some(u) = body_thumb {
        // Reject body-supplied thumbnail URLs that don't point at a
        // YouTube-controlled host. Rendered child-side via <img src>
        // so this is a small-but-real XSS/SSRF surface if a malicious
        // parent injects a forged body. The sidecar-fallback path
        // (when body_thumb is None) is gated by the sidecar's own
        // validation upstream and is trusted.
        if !is_safe_thumbnail_url(u) {
            return Err(AppError::BadRequest(
                "channel_thumbnail_url must be https://*.ytimg.com or \
                 https://*.googleusercontent.com (use sidecar lookup if you \
                 don't have a trusted URL)"
                    .into(),
            ));
        }
    }
    let body_desc = body
        .description
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let body_desc = if let Some(d) = body_desc {
        if d.len() > MAX_DESCRIPTION_LEN {
            // Truncate rather than reject — channel descriptions are
            // best-effort metadata and a legitimate channel with a
            // very long bio shouldn't fail the allowlist add. The
            // length cap is here to bound the column size, not to
            // reject content.
            //
            // **UTF-8 safety**: bytes 8192-onwards may fall mid-
            // codepoint for any description containing multi-byte
            // characters (emoji, CJK, accented Latin). A naive
            // byte-slice `&d[..8192]` would panic with
            // `byte index 8192 is not a char boundary`, which a
            // malicious parent could weaponise as a remote DoS.
            // `char_indices` gives us safe boundaries.
            //
            // `boundary` is the byte offset of the first char that
            // is *not* included in the truncated slice — i.e.,
            // `&d[..boundary]` is the kept portion, and the char
            // starting at exactly `boundary` is dropped. The
            // `take_while(<=)` keeps every char whose *start* byte
            // is at or below the cap; the `last()` of that filtered
            // sequence is therefore the start of the last kept
            // char, which is what we want as the half-open slice
            // upper bound. A description shorter than the cap
            // takes the `else` branch above, so we never need to
            // handle the "every char fits" case here.
            let boundary = d
                .char_indices()
                .map(|(i, _)| i)
                .take_while(|&i| i <= MAX_DESCRIPTION_LEN)
                .last()
                .unwrap_or(0);
            Some(&d[..boundary])
        } else {
            Some(d)
        }
    } else {
        None
    };

    // 2. Only call the sidecar if essential body data is missing.
    //    Title is the gate — if present, we trust the rest of the body too.
    let info = if body_title.is_some() {
        None
    } else {
        let yt = YoutubeClient::from_db(&state.db).await?;
        yt.get_channel(body_channel_id).await.ok().flatten()
    };

    // 3. Combine, preferring body, then sidecar, then error if both empty.
    let title = body_title
        .map(str::to_string)
        .or_else(|| {
            info.as_ref()
                .map(|i| i.title.trim().to_string())
                .filter(|s| !s.is_empty())
        })
        .ok_or_else(|| {
            AppError::BadRequest("channel_title required (sidecar lookup also failed)".into())
        })?;
    let thumb = body_thumb.map(str::to_string).or_else(|| {
        info.as_ref()
            .and_then(|i| preferred_thumbnail(&i.thumbnails))
    });
    let description = body_desc.map(str::to_string).or_else(|| {
        info.as_ref()
            .map(|i| i.description.trim().to_string())
            .filter(|s| !s.is_empty())
    });

    // Use the sidecar's canonical channel ID when available (handle
    // any redirect / disambiguation the sidecar may have applied);
    // otherwise trust the body channel_id.
    let canonical_id = info
        .as_ref()
        .map(|i| i.id.clone())
        .unwrap_or_else(|| body_channel_id.to_string());

    // Seed `channels` and link the per-child `allowlisted_channels` row
    // in one transaction so an observer can never witness the channel
    // metadata row without the linkage that motivated writing it.
    // Failures here are fatal — the FK would reject the allowlist row
    // anyway.
    let mut tx = state.db.begin().await?;
    crate::services::feed_cache::upsert_channel_with_metadata(
        &mut *tx,
        &canonical_id,
        Some(&title),
        thumb.as_deref(),
        description.as_deref(),
    )
    .await?;

    // Insert the linkage; ignore the RETURNING shape — we always
    // re-fetch via the canonical LEFT JOIN below so the response
    // matches `list_channels` exactly regardless of whether this was a
    // fresh insert or a duplicate. Unifying the two paths is cheaper
    // and clearer than the alternative (two correlated subqueries
    // inside `RETURNING` for the channels-table fields).
    sqlx::query(
        "INSERT INTO allowlisted_channels \
            (child_account_id, channel_id, added_by) \
         VALUES (?, ?, ?) \
         ON CONFLICT(child_account_id, channel_id) DO NOTHING",
    )
    .bind(child_id)
    .bind(&canonical_id)
    .bind(current.id)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;

    let row: AllowlistedChannel = sqlx::query_as(
        "SELECT ac.id, ac.channel_id, \
                COALESCE(ch.channel_title, '') AS channel_title, \
                ch.channel_thumbnail_url, ac.created_at \
         FROM allowlisted_channels ac \
         LEFT JOIN channels ch ON ch.channel_id = ac.channel_id \
         WHERE ac.child_account_id = ? AND ac.channel_id = ?",
    )
    .bind(child_id)
    .bind(&canonical_id)
    .fetch_one(&state.db)
    .await?;
    Ok(Json(row))
}

/// `DELETE /api/children/:id/allowlist/channels/:channelId`.
///
/// Performs the allowlist delete and the optional channel_sync_state
/// and channel_videos GC inside a single transaction so an observer
/// can never witness "no child references this channel but
/// channel_sync_state still holds it" — the diagnostics page and the
/// refresher both see a consistent view.
pub async fn delete_channel(
    State(state): State<AppState>,
    Path((child_id, channel_id)): Path<(i64, String)>,
) -> AppResult<StatusCode> {
    require_child_id(&state, child_id).await?;

    let mut tx = state.db.begin().await?;
    sqlx::query("DELETE FROM allowlisted_channels WHERE child_account_id = ? AND channel_id = ?")
        .bind(child_id)
        .bind(&channel_id)
        .execute(&mut *tx)
        .await?;

    // If no other child still has this channel allowlisted, drop the
    // matching `channel_sync_state` row + cascade the `channel_videos`
    // archive so the refresher and the backfill loop stop processing
    // it immediately rather than waiting up to a day for the `feed_gc`
    // cron.
    let still_used: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM allowlisted_channels WHERE channel_id = ?")
            .bind(&channel_id)
            .fetch_one(&mut *tx)
            .await?;
    if still_used == 0 {
        sqlx::query("DELETE FROM channel_videos WHERE channel_id = ?")
            .bind(&channel_id)
            .execute(&mut *tx)
            .await?;
        sqlx::query("DELETE FROM channels WHERE channel_id = ?")
            .bind(&channel_id)
            .execute(&mut *tx)
            .await?;
    }
    tx.commit().await?;
    Ok(StatusCode::NO_CONTENT)
}

// ---------------------------------------------------------------------------
// Videos
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize, sqlx::FromRow)]
pub struct AllowlistedVideo {
    pub id: i64,
    pub video_id: String,
    pub video_title: String,
    pub video_thumbnail_url: Option<String>,
    pub channel_title: Option<String>,
    pub created_at: i64,
}

/// Body for `POST /api/children/:id/allowlist/videos`.
///
/// `video_id` is required; the rest are caller-supplied metadata used
/// **as a fallback** when the discovery sidecar fails to resolve the
/// video. The parent-side allowlist UI already has these fields from
/// the parent search response and passes them through so the row in
/// `allowlisted_videos` always has a non-empty `video_title` (the
/// column the child-side `/api/search` query matches on).
///
/// Sidecar data wins when present and non-empty — the sidecar tends
/// to have canonical, normalised titles. Body data only fills in
/// blanks (e.g. when youtubei.js returns `title: ""`, the video is
/// age-gated, or the network is down).
#[derive(Debug, Deserialize)]
pub struct AddVideoBody {
    pub video_id: String,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub channel_title: Option<String>,
    #[serde(default)]
    pub thumbnail_url: Option<String>,
}

/// `GET /api/children/:id/allowlist/videos`.
pub async fn list_videos(
    State(state): State<AppState>,
    Path(child_id): Path<i64>,
) -> AppResult<Json<Vec<AllowlistedVideo>>> {
    require_child_id(&state, child_id).await?;
    // Hydrate metadata from `videos` (migration 024); fall back to
    // `channels.channel_title` for the per-row `channel_title` display
    // string the API has always exposed.
    let rows: Vec<AllowlistedVideo> = sqlx::query_as(
        "SELECT av.id, av.video_id, \
                v.title AS video_title, \
                v.thumbnail_url AS video_thumbnail_url, \
                ch.channel_title, \
                av.created_at \
         FROM allowlisted_videos av \
         JOIN videos v ON v.video_id = av.video_id \
         LEFT JOIN channels ch ON ch.channel_id = v.channel_id \
         WHERE av.child_account_id = ? \
         ORDER BY av.created_at DESC",
    )
    .bind(child_id)
    .fetch_all(&state.db)
    .await?;
    Ok(Json(rows))
}

/// `POST /api/children/:id/allowlist/videos`.
///
/// Resolves a title / channel / thumbnail for the video by combining
/// (1) the discovery sidecar response and (2) caller-supplied metadata
/// from the body. Both can be missing or partial, but **at least one**
/// must yield a non-empty title — otherwise we'd write a row that the
/// child-side `LIKE` search could never find, which is exactly the
/// bug this endpoint used to ship.
pub async fn add_video(
    State(state): State<AppState>,
    current: CurrentAccount,
    Path(child_id): Path<i64>,
    Json(body): Json<AddVideoBody>,
) -> AppResult<Json<AllowlistedVideo>> {
    require_child_id(&state, child_id).await?;
    let video_id = parse_video_id(&body.video_id);

    // Best-effort sidecar lookup. We deliberately don't propagate
    // sidecar failures — if the caller provided usable metadata we'd
    // rather write a searchable row than 500.
    let yt = YoutubeClient::from_db(&state.db).await?;
    let info = yt.get_video(&video_id).await.ok().flatten();

    // Treat empty strings from the sidecar as "missing" — youtubei.js
    // emits `title: ""` when it can't parse the basic info response.
    let body_title = body
        .title
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let sidecar_title = info
        .as_ref()
        .map(|i| i.title.trim())
        .filter(|s| !s.is_empty());
    let Some(title) = sidecar_title.or(body_title) else {
        return Err(AppError::BadRequest(
            "video not found on YouTube and no title provided".into(),
        ));
    };
    let title = title.to_string();

    // Trim sidecar `channel_title` for consistency with how we treat
    // the sidecar `title` above and the body-supplied `channel_title`
    // below — a whitespace-only value is functionally identical to an
    // empty one and should not be persisted.
    let channel_title = info
        .as_ref()
        .and_then(|i| i.channel_title.as_ref().map(|s| s.trim().to_string()))
        .filter(|s| !s.is_empty())
        .or_else(|| {
            body.channel_title
                .as_ref()
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
        });
    let thumb = info
        .as_ref()
        .and_then(|i| preferred_thumbnail(&i.thumbnails))
        .or_else(|| {
            body.thumbnail_url
                .as_ref()
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
        });
    let canonical_id = info.as_ref().map(|i| i.id.clone()).unwrap_or(video_id);

    // Pull the canonical channel ID from the sidecar response, if any.
    // Body data doesn't currently carry channel_id (just channel_title)
    // so this can be NULL — that's fine; `videos.channel_id` is
    // nullable.
    let channel_id_for_video = info.as_ref().and_then(|i| i.channel_id.clone());
    // `VideoInfo.duration` is ISO 8601 (e.g. "PT4M13S"); parsing it is
    // out of scope for this path. Leave the seconds slot empty — a
    // later sighting (watch_history heartbeat, channel_videos
    // backfill) will fill it in.
    let duration_for_video: Option<i64> = None;

    let mut tx = state.db.begin().await?;
    // Seed `channels` first when we have a resolved channel_id so the
    // `LEFT JOIN channels` in `list_videos` (and the GET response
    // below) returns the actual title rather than NULL on subsequent
    // reads. Without this, a direct-by-ID add would leave the per-row
    // `channel_title` blank until another writer (heartbeat / RSS /
    // backfill) populated `channels`.
    if let Some(cid) = channel_id_for_video.as_deref() {
        crate::services::feed_cache::upsert_channel_with_metadata(
            &mut *tx,
            cid,
            channel_title.as_deref(),
            None,
            None,
        )
        .await?;
    }
    crate::models::video::upsert(
        &mut *tx,
        &canonical_id,
        Some(title.as_str()),
        channel_id_for_video.as_deref(),
        duration_for_video,
        thumb.as_deref(),
    )
    .await?;
    // No-op `DO UPDATE` so `RETURNING id` yields exactly one row in
    // both insert and conflict cases. SQLite's `DO NOTHING ... RETURNING`
    // returns no row on conflict, which would force a fallback SELECT
    // and a second round-trip; rewriting `added_by` back to its own
    // stored value is cheap and keeps the row count contract simple.
    let allow_id: i64 = sqlx::query_scalar(
        "INSERT INTO allowlisted_videos (child_account_id, video_id, added_by) \
         VALUES (?, ?, ?) \
         ON CONFLICT(child_account_id, video_id) \
              DO UPDATE SET added_by = allowlisted_videos.added_by \
         RETURNING id",
    )
    .bind(child_id)
    .bind(&canonical_id)
    .bind(current.id)
    .fetch_one(&mut *tx)
    .await?;
    tx.commit().await?;

    // Re-fetch via the canonical JOIN. The
    // `COALESCE(NULLIF(ch.channel_title, ''), ?)` surfaces the
    // body-supplied channel_title in the response when we couldn't
    // resolve a channel_id (so `channels.channel_title` is NULL) —
    // this is a deliberate, documented asymmetry with `list_videos`:
    // the POST acknowledgement echoes what the caller sent so the
    // optimistic UI can render without a follow-up GET.
    //
    // The `NULLIF(..., '')` guard is defensive: every production
    // writer of `channels.channel_title` filters empties to `None`,
    // but legacy migration-backfilled rows and raw-SQL test fixtures
    // can carry `''`. Plain `COALESCE` treats `''` as "present" and
    // would silently lose the body-supplied fallback in that case.
    //
    // Subsequent GETs (which have no body to fall back on) will show
    // a blank channel_title for the same row until some other writer
    // (heartbeat, RSS, backfill) populates `channels`. Tests in
    // `allowlist_extended.rs::add_video_uses_body_metadata_when_sidecar_unavailable`
    // pin this echo contract.
    let row: AllowlistedVideo = sqlx::query_as(
        "SELECT av.id, av.video_id, v.title AS video_title, \
                v.thumbnail_url AS video_thumbnail_url, \
                COALESCE(NULLIF(ch.channel_title, ''), ?) AS channel_title, \
                av.created_at \
         FROM allowlisted_videos av \
         JOIN videos v ON v.video_id = av.video_id \
         LEFT JOIN channels ch ON ch.channel_id = v.channel_id \
         WHERE av.id = ?",
    )
    .bind(channel_title)
    .bind(allow_id)
    .fetch_one(&state.db)
    .await?;
    Ok(Json(row))
}

/// `DELETE /api/children/:id/allowlist/videos/:videoId`.
pub async fn delete_video(
    State(state): State<AppState>,
    Path((child_id, video_id)): Path<(i64, String)>,
) -> AppResult<StatusCode> {
    require_child_id(&state, child_id).await?;
    sqlx::query("DELETE FROM allowlisted_videos WHERE child_account_id = ? AND video_id = ?")
        .bind(child_id)
        .bind(video_id)
        .execute(&state.db)
        .await?;
    Ok(StatusCode::NO_CONTENT)
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

async fn require_child_id(state: &AppState, child_id: i64) -> AppResult<()> {
    if !access::is_child_account(&state.db, child_id).await? {
        return Err(AppError::BadRequest("target account is not a child".into()));
    }
    Ok(())
}

/// Pick the highest-resolution thumbnail URL we have. YouTube returns
/// keyed sizes; "maxres" → "high" → "medium" → "default" → "standard".
pub(crate) fn preferred_thumbnail(
    thumbs: &std::collections::HashMap<String, crate::services::youtube::ThumbnailInfo>,
) -> Option<String> {
    for key in ["maxres", "high", "standard", "medium", "default"] {
        if let Some(t) = thumbs.get(key) {
            return Some(t.url.clone());
        }
    }
    None
}

/// Accept either a raw video ID or a YouTube URL and return the bare ID.
fn parse_video_id(input: &str) -> String {
    let trimmed = input.trim();
    // youtu.be/<id>
    if let Some(rest) = trimmed.strip_prefix("https://youtu.be/") {
        return rest
            .split(['?', '&', '/'])
            .next()
            .unwrap_or(rest)
            .to_string();
    }
    if let Some(rest) = trimmed.strip_prefix("http://youtu.be/") {
        return rest
            .split(['?', '&', '/'])
            .next()
            .unwrap_or(rest)
            .to_string();
    }
    // youtube.com/watch?v=<id>
    if trimmed.contains("youtube.com/watch") {
        if let Some(qpos) = trimmed.find('?') {
            for part in trimmed[qpos + 1..].split('&') {
                if let Some(v) = part.strip_prefix("v=") {
                    return v.to_string();
                }
            }
        }
    }
    // youtube.com/embed/<id> or shorts/<id>
    for prefix in [
        "https://www.youtube.com/embed/",
        "https://www.youtube.com/shorts/",
        "https://youtube.com/embed/",
        "https://youtube.com/shorts/",
    ] {
        if let Some(rest) = trimmed.strip_prefix(prefix) {
            return rest
                .split(['?', '&', '/'])
                .next()
                .unwrap_or(rest)
                .to_string();
        }
    }
    trimmed.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_safe_thumbnail_url_accepts_real_youtube_hosts() {
        // Real i.ytimg.com URLs from the search response.
        assert!(is_safe_thumbnail_url(
            "https://i.ytimg.com/vi/dQw4w9WgXcQ/hqdefault.jpg"
        ));
        assert!(is_safe_thumbnail_url("https://yt3.ggpht.com/something.jpg"));
        // googleusercontent.com channel avatars
        assert!(is_safe_thumbnail_url(
            "https://yt3.googleusercontent.com/ytc/AAAA.jpg"
        ));
        assert!(is_safe_thumbnail_url(
            "https://lh3.googleusercontent.com/ytc/AAAA.jpg"
        ));
    }

    #[test]
    fn is_safe_thumbnail_url_rejects_arbitrary_hosts() {
        // An attacker-controlled URL embedded in the POST body.
        assert!(!is_safe_thumbnail_url("https://attacker.example/x.jpg"));
        // Lookalike domain (suffix-spoof).
        assert!(!is_safe_thumbnail_url("https://ytimg.com.evil/x.jpg"));
        assert!(!is_safe_thumbnail_url(
            "https://googleusercontent.com.evil/x.jpg"
        ));
        // http:// is not allowed — only https://.
        assert!(!is_safe_thumbnail_url(
            "http://i.ytimg.com/vi/x/hqdefault.jpg"
        ));
        // No scheme.
        assert!(!is_safe_thumbnail_url("i.ytimg.com/vi/x/hqdefault.jpg"));
        // Schemes other than https.
        assert!(!is_safe_thumbnail_url(
            "ftp://i.ytimg.com/vi/x/hqdefault.jpg"
        ));
        assert!(!is_safe_thumbnail_url("javascript:alert(1)//i.ytimg.com"));
    }

    #[test]
    fn is_safe_thumbnail_url_handles_edge_cases() {
        assert!(!is_safe_thumbnail_url(""));
        assert!(!is_safe_thumbnail_url("   "));
        // Length cap.
        let huge = format!("https://i.ytimg.com/{}", "a".repeat(3000));
        assert!(!is_safe_thumbnail_url(&huge));
        // Userinfo is always rejected — even for a legitimate-looking
        // suffix. There's no reason for a thumbnail URL to carry
        // HTTP basic-auth credentials.
        assert!(!is_safe_thumbnail_url(
            "https://user:pass@attacker.example/x.jpg"
        ));
        assert!(
            !is_safe_thumbnail_url("https://user:pass@i.ytimg.com/x.jpg"),
            "userinfo is rejected even on a trusted suffix"
        );
        // Port — accept https://*.ytimg.com:443/...
        assert!(is_safe_thumbnail_url(
            "https://i.ytimg.com:443/vi/x/hqdefault.jpg"
        ));
        // Bare ytimg.com without subdomain is technically YouTube's CDN
        // — accepted.
        assert!(is_safe_thumbnail_url("https://ytimg.com/x.jpg"));
        // Case insensitivity for the host suffix check.
        assert!(is_safe_thumbnail_url(
            "https://I.YTIMG.COM/vi/x/hqdefault.jpg"
        ));
    }

    /// Regression: the host-extraction logic must terminate the
    /// authority at any of `/`, `?`, or `#`. Splitting on `/` alone
    /// would let an attacker hide the real host behind a query or
    /// fragment delimiter — the browser ignores the fragment when
    /// fetching, so `attacker.com#@x.ytimg.com` is fetched as
    /// `attacker.com`. The validator must reject these.
    #[test]
    fn is_safe_thumbnail_url_rejects_fragment_and_query_bypasses() {
        // Fragment-confusion: browser fetches attacker.com.
        assert!(
            !is_safe_thumbnail_url("https://attacker.com#@x.ytimg.com/foo"),
            "fragment bypass must be rejected"
        );
        assert!(
            !is_safe_thumbnail_url("https://attacker.com#/path/that/looks/like.ytimg.com/foo.jpg"),
            "fragment containing apparent-suffix must be rejected"
        );
        // Query-confusion: browser fetches attacker.com?…
        assert!(
            !is_safe_thumbnail_url("https://attacker.com?@x.ytimg.com/foo"),
            "query bypass must be rejected"
        );
        assert!(
            !is_safe_thumbnail_url("https://attacker.com?host=x.ytimg.com"),
            "query string with apparent-suffix must be rejected"
        );

        // Confirm the legitimate fragment/query usage still passes
        // — fragments/queries on a real trusted suffix host should
        // work (the host extraction stops at the delimiter and
        // checks the suffix correctly).
        assert!(
            is_safe_thumbnail_url("https://i.ytimg.com/vi/x/hqdefault.jpg?v=2"),
            "trusted host with a benign query string is fine"
        );
        assert!(
            is_safe_thumbnail_url("https://i.ytimg.com/vi/x/hqdefault.jpg#cache-bust"),
            "trusted host with a benign fragment is fine"
        );
    }
}
