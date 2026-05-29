//! Search helpers.
//!
//! Two distinct concerns share this module:
//!
//! - `GET /api/parent/search` — parent-side discovery, used by the
//!   allowlist UI to find content to add. Backed by the discovery
//!   sidecar. Implemented by [`parent_search`].
//! - `GET /api/search` — child-side allowlist-bounded search.
//!   Implemented by [`child_search`] (Phase 10). The child can only ever
//!   see content that is reachable from their allowlist, and every
//!   query is logged to `search_log` for parent visibility.

use axum::{
    extract::{Query, State},
    Json,
};
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine as _;
use serde::{Deserialize, Serialize};

use crate::error::{AppError, AppResult};
use crate::middleware::auth::CurrentAccount;
use crate::services::access::can_child_view;
use crate::services::youtube::{SearchItem, SearchType, ThumbnailInfo, YoutubeClient};
use crate::state::AppState;

/// Default number of items returned per type.
const DEFAULT_LIMIT: u32 = 20;
/// Hard cap to keep result payloads small.
const MAX_LIMIT: u32 = 50;

/// Child-search query. Extracted to a `const` so the test below can
/// statically assert the inner select uses plain `UNION` (which
/// dedupes by `video_id`) and not `UNION ALL` (which would fan
/// duplicates out to the outer JOIN and produce visible duplicate
/// hits in the response). The `WHERE … LIKE ?2 ESCAPE '\\'` form
/// escapes the LIKE wildcards the caller percent-escapes upstream;
/// the `ESCAPE '\\'` clause is required so a literal `%` or `_` in
/// the user's query string doesn't widen the match.
///
/// Param bind contract (1-indexed via `?N` references for clarity):
/// `?1` = child_account_id (applied to all three inner subqueries),
/// `?2` = LIKE pattern (caller-prepared, includes `%` wildcards),
/// `?3` = LIMIT, `?4` = OFFSET. Do not reorder bindings without
/// updating the `?N` references in the SQL.
const CHILD_SEARCH_SQL: &str = "\
    SELECT v.video_id, v.title AS video_title, \
           v.channel_id, ch.channel_title, \
           v.thumbnail_url AS video_thumbnail_url \
    FROM ( \
       SELECT video_id FROM allowlisted_videos WHERE child_account_id = ?1 \
       UNION \
       SELECT video_id FROM watch_history       WHERE child_account_id = ?1 \
       UNION \
       SELECT cv.video_id \
         FROM channel_videos cv \
         INNER JOIN allowlisted_channels ac ON ac.channel_id = cv.channel_id \
         WHERE ac.child_account_id = ?1 AND cv.is_deleted = 0 \
    ) src \
    JOIN videos v ON v.video_id = src.video_id \
    LEFT JOIN channels ch ON ch.channel_id = v.channel_id \
    WHERE v.title LIKE ?2 ESCAPE '\\' \
    ORDER BY v.title \
    LIMIT ?3 OFFSET ?4";

#[derive(Debug, Deserialize)]
pub struct ParentSearchQuery {
    pub q: String,
    /// `"channel"` or `"video"`.
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(default)]
    pub max_results: Option<u32>,
}

#[derive(Debug, Serialize)]
pub struct SearchResponse {
    pub items: Vec<SearchItem>,
}

/// `GET /api/parent/search?q=&type=channel|video`.
pub async fn parent_search(
    State(state): State<AppState>,
    Query(q): Query<ParentSearchQuery>,
) -> AppResult<Json<SearchResponse>> {
    let kind = SearchType::parse(&q.kind)
        .ok_or_else(|| AppError::BadRequest("type must be channel|video".into()))?;
    let yt = YoutubeClient::from_db(&state.db).await?;
    let items = yt.search(&q.q, kind, q.max_results.unwrap_or(15)).await?;
    Ok(Json(SearchResponse { items }))
}

// ---------------------------------------------------------------------------
// Child search
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct ChildSearchQuery {
    pub q: String,
    /// One of `channel`, `video`, or `all` (default).
    #[serde(default, rename = "type")]
    pub kind: Option<String>,
    /// Optional pagination cursor returned in a previous response's
    /// `next_page_token` field. The token is an opaque base64url-encoded
    /// JSON object of the form `{"offset": N}` and is applied uniformly
    /// to every result bucket (channels / videos). When absent we
    /// start at offset 0.
    #[serde(default)]
    pub page_token: Option<String>,
    #[serde(default)]
    pub limit: Option<u32>,
}

/// Decoded form of `page_token`. Shared with `channels::list_videos`
/// via the `pub(crate)` visibility so the two paginated endpoints
/// produce the same opaque-token shape.
#[derive(Debug, Serialize, Deserialize, Default)]
pub(crate) struct PageCursor {
    /// Number of rows already returned in earlier pages.
    pub(crate) offset: i64,
}

pub(crate) fn decode_page_token(token: &str) -> Option<PageCursor> {
    let decoded = URL_SAFE_NO_PAD.decode(token.as_bytes()).ok()?;
    serde_json::from_slice(&decoded).ok()
}

pub(crate) fn encode_page_token(cursor: &PageCursor) -> String {
    let json = serde_json::to_vec(cursor).unwrap_or_else(|_| b"{}".to_vec());
    URL_SAFE_NO_PAD.encode(json)
}

#[derive(Debug, Serialize, Clone)]
pub struct ChildChannelHit {
    pub channel_id: String,
    pub channel_title: String,
    pub channel_thumbnail_url: Option<String>,
}

#[derive(Debug, Serialize, Clone)]
pub struct ChildVideoHit {
    pub video_id: String,
    pub title: String,
    pub channel_id: Option<String>,
    pub channel_title: Option<String>,
    pub thumbnail_url: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct ChildSearchResults {
    pub channels: Vec<ChildChannelHit>,
    pub videos: Vec<ChildVideoHit>,
}

#[derive(Debug, Serialize)]
pub struct ChildSearchResponse {
    pub q: String,
    pub kind: String,
    pub results: ChildSearchResults,
    pub next_page_token: Option<String>,
}

/// `GET /api/search` — child-only, allowlist-bounded search.
///
/// Searches across:
///
/// - **Channels** the child can reach via `allowlisted_channels` or
///   their subscriptions.
/// - **Videos** in `allowlisted_videos`, `watch_history`, and the
///   recent uploads cache for allowlisted channels.
///
/// The query is logged to `search_log` regardless of result count so
/// parents can see what their child is searching for.
pub async fn child_search(
    State(state): State<AppState>,
    current: CurrentAccount,
    Query(q): Query<ChildSearchQuery>,
) -> AppResult<Json<ChildSearchResponse>> {
    // The route is gated by `require_child` middleware in
    // [`crate::routes::router`], so we don't re-check the role here.

    let trimmed = q.q.trim();
    if trimmed.is_empty() {
        return Err(AppError::BadRequest("q is required".into()));
    }
    let kind_label = q.kind.clone().unwrap_or_else(|| "all".to_string());
    let limit = q.limit.unwrap_or(DEFAULT_LIMIT).min(MAX_LIMIT) as i64;
    let pattern = format!("%{}%", escape_like_pattern(trimmed));

    let cursor = q
        .page_token
        .as_deref()
        .and_then(decode_page_token)
        .unwrap_or_default();
    let offset = cursor.offset.max(0);

    let mut results = ChildSearchResults {
        channels: Vec::new(),
        videos: Vec::new(),
    };

    let want_channels = matches!(kind_label.as_str(), "channel" | "all");
    let want_videos = matches!(kind_label.as_str(), "video" | "all");

    if want_channels {
        results.channels = search_channels(&state, current.id, &pattern, limit, offset).await?;
    }
    if want_videos {
        results.videos = search_videos(&state, current.id, &pattern, limit, offset).await?;
    }

    // Apply access control to every video hit so a blocked-then-
    // allowlisted edge case still hides the video. The channel ID is
    // enough to keep the channel-allowlist branch alive.
    let mut filtered_videos = Vec::with_capacity(results.videos.len());
    for hit in results.videos.drain(..) {
        if can_child_view(
            &state.db,
            current.id,
            &hit.video_id,
            hit.channel_id.as_deref(),
        )
        .await
        .unwrap_or(false)
        {
            filtered_videos.push(hit);
        }
    }
    results.videos = filtered_videos;

    let total = results.channels.len() + results.videos.len();

    // Always log, regardless of result count. Only log the first page so
    // a single search session doesn't produce duplicate `search_log`
    // rows on every "load more" request.
    if offset == 0 {
        // Detect "first time we've ever seen this query for this child"
        // *before* writing the new row, so we can dispatch a
        // `new_search_term` notification.
        let prior: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM search_log WHERE child_account_id = ? AND query = ?",
        )
        .bind(current.id)
        .bind(trimmed)
        .fetch_one(&state.db)
        .await
        .unwrap_or(0);

        let _ = sqlx::query(
            "INSERT INTO search_log (child_account_id, query, result_count) VALUES (?, ?, ?)",
        )
        .bind(current.id)
        .bind(trimmed)
        .bind(total as i64)
        .execute(&state.db)
        .await;

        if prior == 0 {
            let display_name: String =
                sqlx::query_scalar("SELECT display_name FROM accounts WHERE id = ?")
                    .bind(current.id)
                    .fetch_one(&state.db)
                    .await
                    .unwrap_or_else(|_| "A child".to_string());
            let _ = crate::services::notifications::dispatch_new_search_term(
                &state.db,
                current.id,
                &display_name,
                trimmed,
            )
            .await;
        }
    }

    // Emit a `next_page_token` only if any individual bucket appears to
    // be saturated at the per-bucket `limit`. A bucket with strictly
    // fewer rows than `limit` has been fully drained.
    let has_more = results.channels.len() as i64 >= limit || results.videos.len() as i64 >= limit;
    let next_page_token = has_more.then(|| {
        encode_page_token(&PageCursor {
            offset: offset + limit,
        })
    });

    Ok(Json(ChildSearchResponse {
        q: trimmed.to_string(),
        kind: kind_label,
        results,
        next_page_token,
    }))
}

async fn search_channels(
    state: &AppState,
    child_id: i64,
    pattern: &str,
    limit: i64,
    offset: i64,
) -> AppResult<Vec<ChildChannelHit>> {
    // Two sources of channel visibility:
    //   1. Direct allowlist
    //   2. The child's subscribed channels (which the child can browse
    //      via /child/channels regardless of the per-video allowlist)
    //
    // We UNION on the (channel_id, channel_title, thumbnail) shape so
    // the result is naturally deduplicated.
    // `allowlisted_channels` is now slim (FK only); pull metadata from
    // `channels`. `child_subscriptions` still carries its own
    // denormalised columns (out of scope for this refactor).
    //
    // Allowlisted channels missing a `channels.channel_title` are
    // intentionally excluded from search via `INNER JOIN channels`:
    // a `LEFT JOIN` + `COALESCE(..., '')` would still silently drop
    // them at the outer `LIKE` step (an empty string can never match a
    // non-empty pattern), but the inner-join form makes the exclusion
    // explicit and saves the COALESCE. The allowlist POST handler
    // upserts `channels` synchronously, and migration 025 seeds a row
    // for every allowlisted channel — so in practice this affects no
    // rows. If it ever does, the fix is to populate `channels`, not to
    // surface a blank-title row that the user can't search by name.
    let rows: Vec<(String, String, Option<String>)> = sqlx::query_as(
        "SELECT channel_id, channel_title, channel_thumbnail_url FROM ( \
            SELECT ac.channel_id, \
                   ch.channel_title AS channel_title, \
                   ch.channel_thumbnail_url \
              FROM allowlisted_channels ac \
              JOIN channels ch ON ch.channel_id = ac.channel_id \
              WHERE ac.child_account_id = ? \
                AND ch.channel_title IS NOT NULL \
            UNION \
            SELECT channel_id, channel_title, channel_thumbnail_url \
              FROM child_subscriptions \
              WHERE child_account_id = ? AND is_deleted = 0 \
         ) \
         WHERE channel_title LIKE ? ESCAPE '\\' \
         ORDER BY channel_title \
         LIMIT ? OFFSET ?",
    )
    .bind(child_id)
    .bind(child_id)
    .bind(pattern)
    .bind(limit)
    .bind(offset)
    .fetch_all(&state.db)
    .await?;
    Ok(rows
        .into_iter()
        .map(|(id, title, thumb)| ChildChannelHit {
            channel_id: id,
            channel_title: title,
            channel_thumbnail_url: thumb,
        })
        .collect())
}

async fn search_videos(
    state: &AppState,
    child_id: i64,
    pattern: &str,
    limit: i64,
    offset: i64,
) -> AppResult<Vec<ChildVideoHit>> {
    // We search local cached metadata first to keep the request fast.
    // Three local sources, gathered via UNION ALL + a per-video GROUP BY:
    //
    //   1. allowlisted_videos       — direct per-video allowlist
    //   2. watch_history            — videos already proven viewable
    //   3. channel_videos via       — videos surfaced through an
    //      allowlisted_channels       allowlisted channel's archive
    //
    // (3) is necessary because a child whose access derives *only*
    // from a channel allowlist would otherwise be unable to search
    // for those videos at all. `channel_videos` holds the full
    // archive populated by RSS + yt-dlp backfill.
    //
    // We now project the inner UNION over `video_id` only (a single
    // column), then JOIN to `videos`/`channels` outside for the
    // canonical metadata. With one column in the projection, plain
    // `UNION` correctly dedupes by `video_id` — there's no NULL-vs-
    // real `channel_id` distinction inside the inner select to
    // preserve. The earlier `UNION ALL + GROUP BY + MAX(channel_id)`
    // shape (preserved in older revisions of this comment) was a
    // workaround for a multi-column projection; under the current
    // single-column shape plain `UNION` is both simpler and
    // correct.
    type SearchRow = (
        String,
        String,
        Option<String>,
        Option<String>,
        Option<String>,
    );
    // Three sources of reachable videos (per-child allowlist, watch
    // history, and the channel-allowlist archive) get UNIONed; we then
    // join the resulting `video_id`s against `videos` + `channels` so
    // the title/thumbnail/channel-title come from the canonical store.
    //
    // CONTRACT: the inner `UNION` (NOT `UNION ALL`) deduplicates by
    // `video_id`, which is what the outer `JOIN videos v` relies on to
    // produce exactly one row per match. A `video_id` that appears in
    // both `allowlisted_videos` and `watch_history` (common case) must
    // not fan out to two response rows. Do NOT swap `UNION` for
    // `UNION ALL` here without also wrapping the outer select in
    // `DISTINCT` / `GROUP BY v.video_id`.
    // Propagate query/mapping failures (matching `search_channels`)
    // rather than `unwrap_or_default()`-ing them: a SQL or decode error
    // is a real server fault, and swallowing it as an empty result is
    // indistinguishable from "no matches" to the caller.
    let rows: Vec<SearchRow> = sqlx::query_as(CHILD_SEARCH_SQL)
        .bind(child_id)
        .bind(pattern)
        .bind(limit)
        .bind(offset)
        .fetch_all(&state.db)
        .await?;

    Ok(rows
        .into_iter()
        .map(
            |(video_id, title, channel_id, ch_title, thumb)| ChildVideoHit {
                video_id,
                title,
                // Surfaced for `can_child_view`'s channel-allowlist branch
                // in the layer-B post-filter (see `child_search`).
                channel_id,
                channel_title: ch_title,
                thumbnail_url: thumb,
            },
        )
        .collect())
}

/// Escape user input for a SQL `LIKE … ESCAPE '\\'` pattern.
///
/// Order matters: the escape character itself (`\`) must be escaped
/// FIRST. Escaping `%` or `_` first would inject fresh backslashes,
/// and a user-supplied `\` immediately before that fresh backslash
/// would then form `\\` (literal backslash) under `ESCAPE '\\'`,
/// leaving the wildcard UN-escaped — defeating the documented
/// "literal `%` / `_` / `\` in the user's query doesn't widen the
/// match" promise.
fn escape_like_pattern(input: &str) -> String {
    input
        .replace('\\', "\\\\")
        .replace('%', "\\%")
        .replace('_', "\\_")
}

/// Helper: pick the highest-resolution thumbnail URL from a YouTube
/// `thumbnails` map. Currently unused locally but exposed for future
/// suggestion endpoints.
#[allow(dead_code)]
pub fn pick_thumb_url(thumbs: &std::collections::HashMap<String, ThumbnailInfo>) -> Option<String> {
    for key in ["maxres", "high", "standard", "medium", "default"] {
        if let Some(t) = thumbs.get(key) {
            return Some(t.url.clone());
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn child_search_sql_uses_plain_union_not_union_all() {
        // Regression guard: `UNION ALL` in the inner subselect would
        // fan duplicates (e.g. a video in both `allowlisted_videos`
        // and `watch_history`) out to the outer JOIN and produce
        // duplicate response rows. Plain `UNION` dedupes by the
        // single-column `video_id` projection. Catch a future
        // regression statically rather than via a hard-to-reproduce
        // integration test against duplicate fixtures.
        assert!(
            !CHILD_SEARCH_SQL.contains("UNION ALL"),
            "child_search SQL must use UNION, not UNION ALL; see docstring"
        );
        // Sanity: still contains a UNION at all (so a refactor that
        // dropped the dedupe entirely also flips this red).
        assert!(
            CHILD_SEARCH_SQL.contains("UNION"),
            "child_search SQL must contain UNION for cross-source dedupe"
        );
    }

    #[test]
    fn escape_like_pattern_escapes_backslash_before_wildcards() {
        // Plain wildcards are escaped.
        assert_eq!(escape_like_pattern("100%"), "100\\%");
        assert_eq!(escape_like_pattern("a_b"), "a\\_b");
        // Literal backslash is escaped FIRST so it can't be "consumed"
        // by the escape we inject for a following `%` or `_`.
        assert_eq!(escape_like_pattern("foo\\bar"), "foo\\\\bar");
        // The critical regression case: a user-supplied `\` immediately
        // before a `%` must NOT result in an un-escaped wildcard.
        // Expected: `foo\\\%bar` = literal `\` + escaped `%`.
        assert_eq!(escape_like_pattern("foo\\%bar"), "foo\\\\\\%bar");
        // Same for `_`.
        assert_eq!(escape_like_pattern("foo\\_bar"), "foo\\\\\\_bar");
    }
}
