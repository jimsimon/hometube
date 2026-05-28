//! Shared `videos` reference table.
//!
//! Introduced by migration 024 to consolidate the per-video metadata
//! (`title`, `channel_id`, `duration_seconds`, `thumbnail_url`) that
//! used to be duplicated across `allowlisted_videos`, `blocked_videos`,
//! `hidden_videos`, `watch_history`, `video_likes`, `offline_downloads`,
//! and `channel_videos`.
//!
//! Every route that previously denormalised those columns now calls
//! [`upsert`] first, then inserts a slim FK-only row into the per-child
//! table. Listings JOIN back to `videos` for display.

use sqlx::SqliteExecutor;

use crate::error::AppResult;

/// Upsert a row into `videos`.
///
/// Refreshes any non-empty field on conflict so YouTube renames /
/// thumbnail-URL changes propagate across every per-child surface.
/// Empty strings are treated as "missing" so callers can pass through
/// the optional sidecar/body payloads without bespoke filtering.
///
/// `title` is `Option<&str>` so callers can explicitly signal "I don't
/// know the title yet" without colliding with the `videos.title NOT NULL`
/// constraint. On the INSERT path, a missing title falls back to the
/// `video_id` so the row is creatable; on the CONFLICT path, the
/// caller's `None` is preserved as SQL `NULL` and COALESCEs against the
/// stored value so a previously-stored richer title is never clobbered
/// by a placeholder.
///
/// **Placeholder convention:** when a `None`-title call creates the row,
/// `title == video_id` is the placeholder. Downstream consumers that
/// JOIN `videos` and care about display can treat `title == video_id`
/// as "not yet enriched"; a subsequent sighting (heartbeat, channel
/// backfill, etc.) replaces it with the real title.
pub async fn upsert<'e, E>(
    exec: E,
    video_id: &str,
    title: Option<&str>,
    channel_id: Option<&str>,
    duration_seconds: Option<i64>,
    thumbnail_url: Option<&str>,
) -> AppResult<()>
where
    E: SqliteExecutor<'e>,
{
    // INSERT path needs a non-null title; fall back to the `video_id`
    // so the NOT NULL constraint is satisfied. The CONFLICT path binds
    // `title` (the Option) directly as ?6 so `None` arrives as SQL NULL
    // and COALESCE picks the stored value.
    //
    // BIND ORDER CONTRACT: the CONFLICT SET clause reuses ?3, ?4, ?5
    // from the VALUES list (channel_id, duration_seconds, thumbnail_url)
    // and adds ?6 for the raw `Option`-typed title. Do NOT reorder the
    // `.bind(...)` chain below without also renumbering the `?N`
    // placeholders — SQLite resolves positional parameters by their
    // index, so a reorder silently produces wrong upserts (e.g. binding
    // `duration_seconds` where `channel_id` is expected).
    //
    // INSERT-side NULLIF: every production caller already filters
    // empty strings to `None` before calling, so the INSERT bind for
    // `?3`/`?5` is either `None` or a real value. We don't wrap the
    // INSERT VALUES in `NULLIF` because (a) it would mask a caller
    // that started passing `Some("")` (those are bugs we want to
    // catch in tests via blank-title surfaces) and (b) the CONFLICT
    // path's NULLIF is the rename-propagation contract, not a
    // safety net for the INSERT branch.
    let insert_title = title.unwrap_or(video_id);
    sqlx::query(
        "INSERT INTO videos (video_id, title, channel_id, duration_seconds, thumbnail_url) \
         VALUES (?1, ?2, ?3, ?4, ?5) \
         ON CONFLICT(video_id) DO UPDATE SET \
            title            = COALESCE(NULLIF(?6, ''), videos.title), \
            channel_id       = COALESCE(NULLIF(?3, ''), videos.channel_id), \
            duration_seconds = COALESCE(?4, videos.duration_seconds), \
            thumbnail_url    = COALESCE(NULLIF(?5, ''), videos.thumbnail_url), \
            last_updated_at  = unixepoch()",
    )
    .bind(video_id) //          ?1
    .bind(insert_title) //      ?2
    .bind(channel_id) //        ?3 (reused by CONFLICT)
    .bind(duration_seconds) //  ?4 (reused by CONFLICT)
    .bind(thumbnail_url) //     ?5 (reused by CONFLICT)
    .bind(title) //             ?6 (CONFLICT-only: raw Option<&str>)
    .execute(exec)
    .await?;
    Ok(())
}

/// Upsert a `videos` row using only a `video_id`. Used when a route
/// (e.g. `allowlist_videos.add`) discovers a video but has nothing else
/// to record yet; later sightings will fill in the metadata via
/// [`upsert`]. The placeholder title falls back to the `video_id` so the
/// `NOT NULL` constraint is satisfied without polluting display
/// surfaces with empty strings.
pub async fn upsert_stub<'e, E>(exec: E, video_id: &str) -> AppResult<()>
where
    E: SqliteExecutor<'e>,
{
    sqlx::query(
        "INSERT INTO videos (video_id, title) VALUES (?, ?) \
         ON CONFLICT(video_id) DO NOTHING",
    )
    .bind(video_id)
    .bind(video_id)
    .execute(exec)
    .await?;
    Ok(())
}
