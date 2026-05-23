# Full-channel video-list sync (channel backfill)

## Goal

For every channel allowlisted by a parent — present and future — maintain a complete, periodically-refreshed list of that channel's uploaded videos in the local SQLite, without tripping YouTube's anti-bot detection.

Eligibility is **allowlist-only**: as soon as a parent allowlists a channel, both sync tiers (RSS freshness + yt-dlp backfill) become eligible for that channel. Subscriptions are a *priority hint* — a child subscribing to an allowlisted channel that hasn't been backfilled yet bumps it to the front of the queue. Otherwise, subscriptions are unrelated to backfill.

Rationale for allowlist-only over allowlist-AND-subscribe: the backfill loop's consumption rate is a fixed 1 channel/hour. Queue depth can grow freely without affecting YouTube call rate, so there is no anti-bot benefit to filtering eligibility further. Search/discovery use-cases (e.g. a child finding a video before they've subscribed to the channel) benefit from having the archive ready as soon as the parent says "yes, this channel is OK".

## Three-tier sync strategy

The system gains a unified `channel_videos` table that consolidates the existing per-channel hot cache (`feed_source_items`, capped at 20/source) and the new full-archive store. Three layers write to it, each with a different role:

| Tier | Trigger | Transport | Cadence | Anti-bot risk | Coverage |
|---|---|---|---|---|---|
| **Bootstrap** | First time a channel becomes eligible (allowlisted) | yt-dlp `--flat-playlist` | One-shot | Medium (cookies + PO token) | Full archive |
| **Freshness** | Existing `feed_refresher` | RSS (cheap, anti-bot-safe) → InnerTube sidecar fallback on RSS failure | Hourly per channel | Low (RSS) / Medium (sidecar fallback) | ~15 newest uploads |
| **Reconciliation** | Periodic re-backfill | yt-dlp `--flat-playlist` | 30 days per channel | Medium | Full archive + diff against tombstoned rows |

This explicitly rejects the "drop RSS, use yt-dlp for everything" alternative. RSS is the cheapest, lowest-risk transport in the codebase — `src/services/youtube_rss.rs:6` notes it "does not go through the InnerTube anti-bot path", supports ETag/If-Modified-Since 304s, and gives near-real-time freshness for new uploads. Replacing it with yt-dlp would *increase* YouTube load and anti-bot exposure.

Conversely, the 20-item-per-source cap in `feed_source_items` (`src/services/feed_cache.rs:22`) is purely a self-limitation of the hot cache — it has nothing to do with YouTube. Unifying onto `channel_videos` lets us drop both the cap and the parallel `feed_source_items` table without changing YouTube query volume.

## Why yt-dlp `--flat-playlist`

Both the discovery sidecar (`youtubei.js`) and `yt-dlp` end up at YouTube's InnerTube API, but the anti-bot envelopes are not equivalent:

| Surface | Cookies | PO token | Multi-client args | n-param JS | Continuations |
|---|---|---|---|---|---|
| `youtubei.js` sidecar (current) | no | no | no | n/a | dropped at HTTP boundary (`sidecar/discovery/server.js:280-287`) |
| `yt-dlp` (`src/services/ytdlp.rs`) | yes (`data/tools/cookies.txt`) | yes (bgutil + `pot-server:4416`) | yes (`default,ios,web`) | yes (Deno) | native, in-process |

For paginating thousands of videos per channel, yt-dlp's envelope is the only one already battle-tested in this codebase. It also collapses the entire channel into a single subprocess (~one inbound stream of `--flat-playlist` JSON lines) so we don't have to teach the sidecar to hold continuation state across HTTP calls.

Command shape:

```
yt-dlp \
  --flat-playlist --skip-download \
  --print-json \
  --extractor-args "youtubetab:approximate_date" \
  --cookies <tempfile> \
  --plugin-dirs <bgutil-plugin> \
  --extractor-args "youtubepot-bgutilhttp:base_url=<POT_SERVER_URL>" \
  --no-warnings \
  --sleep-requests 1 --sleep-interval 1 --max-sleep-interval 3 \
  https://www.youtube.com/channel/<UCID>/videos
```

Each line is a video stub: `id`, `title`, `duration` (often null on flat), `view_count` (often null), `upload_date` (with `approximate_date`), `channel_id`, `channel`. We do **not** request stream info; that stays on the per-video `--dump-json` path that already exists.

## Architecture

```
                                   ┌─────────────────────────────────────────┐
                                   │  channel_sync_state                     │
                                   │  (per-channel state, both tiers)        │
                                   └────────────────┬────────────────────────┘
                                                    │ claim_due()
                                                    ▼
  allowlisted_channels ──seed──►  channel_backfiller (new background task)
       (on POST + startup;             ▲   │
        subscribe = priority hint) ────┘   │
                                           │  spawn yt-dlp --flat-playlist
                                           ▼
                                  ChannelVideosListing → upsert channel_videos
                                                       └──► dispatch failures
                                                            via parent_notifications
```

Reuses, idiomatically, the patterns from `feed_refresher`:

- Atomic lease via `UPDATE … RETURNING` (`feed_cache::claim_due_sources` pattern).
- `backfill_next_at` with exponential backoff + jitter on failure.
- Live-reloadable tunables persisted in `app_config`, surfaced through `/api/admin/feed-refresher/settings`-style routes.
- Notification on persistent failure (new `channel_backfill_error` type).

But it is a **separate** loop — different cadence, different concurrency budget, different anti-bot pacing.

## Database changes — migration `020_channel_backfill.sql`

### `channel_videos` (new) — unified video store

The single source of truth for "what videos exist on this channel". Written to by RSS, the InnerTube sidecar fallback, and yt-dlp backfill. Read by the New Videos feed, the channel archive endpoint, and (eventually) the parent UI.

```sql
CREATE TABLE channel_videos (
    channel_id      TEXT NOT NULL,
    video_id        TEXT NOT NULL,
    title           TEXT NOT NULL,
    channel_title   TEXT,                      -- denormalised; matches feed_source_items convention
    published_at    INTEGER,                   -- unix seconds, may be approximate
    published_raw   TEXT,                      -- raw upload_date / RSS <published> as-given
    duration_s      INTEGER,                   -- nullable; yt-dlp may supply, RSS does not
    view_count      INTEGER,                   -- nullable; yt-dlp may supply, RSS does not
    thumbnail_url   TEXT,                      -- RSS-supplied or derived (i.ytimg.com/vi/<id>/hqdefault.jpg)
    first_seen_at   INTEGER NOT NULL,          -- set on insert, never updated
    last_seen_at    INTEGER NOT NULL,          -- bumped by every successful sighting (RSS, sidecar, or backfill)
    source          TEXT NOT NULL              -- most recent writer
                     CHECK (source IN ('rss', 'sidecar', 'backfill')),
    is_deleted      INTEGER NOT NULL DEFAULT 0,-- 1 = backfill reconciliation no longer lists it
    PRIMARY KEY (channel_id, video_id)
);
CREATE INDEX idx_channel_videos_channel_published
    ON channel_videos(channel_id, published_at DESC);
CREATE INDEX idx_channel_videos_last_seen
    ON channel_videos(last_seen_at);
CREATE INDEX idx_channel_videos_not_deleted_published
    ON channel_videos(channel_id, is_deleted, published_at DESC);
```

Notes:
- No FK to a `channels` table (none exists; the codebase keeps channel identity denormalised).
- `is_deleted=1` is set only by **backfill reconciliation**, never by RSS or sidecar (see "Write-path semantics" below).
- Thumbnails: RSS supplies a real URL in `<media:thumbnail>`; yt-dlp flat-playlist gives an id we map to `https://i.ytimg.com/vi/<id>/hqdefault.jpg`. Falls back to `mqdefault.jpg` at render time if `hqdefault` 404s.
- `source` column is informational/diagnostic — useful for "which path last touched this row" admin views; not part of any query predicate.

### `feed_source_items` — **dropped** in this migration

```sql
DROP TABLE feed_source_items;
```

But first, migrate existing rows so we don't lose the current hot cache:

```sql
INSERT INTO channel_videos
    (channel_id, video_id, title, channel_title, published_at, published_raw,
     thumbnail_url, first_seen_at, last_seen_at, source, is_deleted)
SELECT
    COALESCE(channel_id, source_id),
    video_id, title, channel_title, published_at, published_raw,
    thumbnail_url, fetched_at, fetched_at, 'rss', 0
FROM feed_source_items
WHERE kind = 'channel'
ON CONFLICT(channel_id, video_id) DO NOTHING;
```

The `feed_sources` table is **consolidated into a new `channel_sync_state` table** in this same migration — see below.

### Write-path semantics

| Writer | On hit | On miss (video in DB but not in this fetch) |
|---|---|---|
| **RSS** (`feed_refresher` → `upsert_channel_videos_from_rss`) | Upsert: insert with `first_seen_at=now`, `is_deleted=0`, `source='rss'`; or update `last_seen_at=now`, `source='rss'`, `is_deleted=0` (RSS sighting clears any prior tombstone). Do NOT touch `duration_s`/`view_count`. | Do **nothing**. RSS only sees ~15 newest items; absence from RSS is not evidence of deletion. |
| **Sidecar fallback** (`feed_refresher` → `upsert_channel_videos_from_sidecar`) | Same as RSS: upsert + bump + clear-tombstone, `source='sidecar'`. | Do **nothing**. Sidecar `/channel-videos` is single-page (~30 items max). |
| **yt-dlp backfill** (`channel_backfill::run_backfill_for`) | Upsert: insert with `first_seen_at=now`, `is_deleted=0`, `source='backfill'`; or update `last_seen_at=now`, `source='backfill'`, `is_deleted=0`. Populate `duration_s`/`view_count` if yt-dlp supplied them. | **Reconcile**: set `is_deleted=1` for any row where `channel_id=X` AND `first_seen_at < backfill_started_at` AND not seen in this run. The `first_seen_at < backfill_started_at` clause is critical — it avoids tombstoning rows that RSS upserted *during* the backfill run (newer than the backfill's view). |

The reconciliation rule is the key correctness property: RSS-fed rows that haven't yet been backfill-witnessed are immune to tombstoning until the next backfill cycle. After that, any row genuinely gone from the channel's uploads playlist will fail to be re-witnessed and get tombstoned.

### `channel_sync_state` (new) — consolidates `feed_sources` + backfill state + channel header metadata

Today `feed_sources` is named for a historical model that no longer applies (its `kind` was constrained to `'channel'` only in migration 017). Its actual content is "per-channel state for the freshness sync tier". Adding a separate `channel_backfill_state` table would split the same conceptual entity — *one channel, all the state we track for it* — across two tables keyed identically (`channel_id`) with the same lifecycle (eligible iff allowlisted) and the same GC trigger.

Consolidate into one table:

```sql
CREATE TABLE channel_sync_state (
    channel_id                       TEXT PRIMARY KEY,

    -- Channel header metadata (served by GET /api/channels/:channelId)
    channel_title                    TEXT,
    channel_thumbnail_url            TEXT,
    description                      TEXT,
    -- (subscriber_count intentionally omitted — see "Drop subscriber_count" section below)

    -- Freshness tier (RSS + InnerTube sidecar fallback) — formerly feed_sources columns
    rss_etag                         TEXT,
    rss_last_modified                TEXT,
    rss_last_polled_at               INTEGER,
    rss_last_success_at              INTEGER,
    rss_last_error                   TEXT,
    rss_consecutive_errors           INTEGER NOT NULL DEFAULT 0,
    rss_next_poll_at                 INTEGER NOT NULL DEFAULT 0,
    sidecar_last_fallback_at         INTEGER,

    -- Backfill tier (yt-dlp --flat-playlist) — new
    backfill_status                  TEXT NOT NULL DEFAULT 'pending'
                                      CHECK (backfill_status IN ('pending','running','complete','failed','shelved')),
    backfill_last_started_at         INTEGER,
    backfill_last_completed_at       INTEGER,
    backfill_last_attempted_at       INTEGER,
    backfill_next_at                 INTEGER NOT NULL DEFAULT 0,
    backfill_lease_expires_at        INTEGER,
    backfill_last_error              TEXT,
    backfill_consecutive_errors      INTEGER NOT NULL DEFAULT 0,
    backfill_videos_observed_total   INTEGER NOT NULL DEFAULT 0,
    backfill_videos_new_last_run     INTEGER NOT NULL DEFAULT 0,
    backfill_videos_removed_last_run INTEGER NOT NULL DEFAULT 0
);

-- One index per claim_due query path
CREATE INDEX idx_channel_sync_rss_next      ON channel_sync_state(rss_next_poll_at);
CREATE INDEX idx_channel_sync_backfill_next ON channel_sync_state(backfill_next_at)
    WHERE backfill_status != 'shelved';

-- Migrate existing feed_sources rows.
-- channel_thumbnail_url and description populate lazily:
--   - thumbnail_url is already on allowlisted_channels; we backfill it from there below.
--   - description fills in on next allowlist re-add or sidecar fallback (not strictly required;
--     existing pre-upgrade channels just have a NULL description until then).
INSERT INTO channel_sync_state
    (channel_id, channel_title,
     rss_etag, rss_last_modified, rss_last_polled_at, rss_last_success_at,
     rss_last_error, rss_consecutive_errors, rss_next_poll_at,
     sidecar_last_fallback_at,
     backfill_status, backfill_next_at)
SELECT
    source_id, title,
    etag, last_modified, last_polled_at, last_success_at,
    last_error, consecutive_errors, next_poll_at,
    last_sidecar_fallback_at,
    'pending', 0
FROM feed_sources
WHERE kind = 'channel';

-- Backfill channel_thumbnail_url from allowlisted_channels (already populated for existing rows).
UPDATE channel_sync_state
SET channel_thumbnail_url = (
    SELECT MAX(channel_thumbnail_url) FROM allowlisted_channels
    WHERE allowlisted_channels.channel_id = channel_sync_state.channel_id
)
WHERE channel_thumbnail_url IS NULL;

DROP TABLE feed_sources;
```

Why this is fine (no contention concern):
- The two tier loops (`feed_refresher` and `channel_backfill`) write disjoint column subsets.
- Their `claim_due` queries hit different indexes (`idx_channel_sync_rss_next` vs `idx_channel_sync_backfill_next`).
- SQLite WAL handles per-row updates with no extra concurrency cost.

Operational wins:
- One row per channel; single GC delete on un-allowlist.
- One eligibility reconcile query covers both tiers.
- Admin diagnostics page can show all sync state for a channel in one row.
- Symmetric naming (`rss_…` / `backfill_…`) keeps the schema self-documenting.

The old name "`feed_sources`" was a legacy from when the design allowed multiple `kind` values (channels + playlists). With migration 017 having already locked `kind` to `'channel'`, the entity has been "the channels we sync" for a while; the table just hadn't been renamed.

`backfill_status` transitions:
- `pending` → `running` (on claim) → `complete` (on full pass) → `pending` (after `re_backfill_interval`)
- `running` → `failed` (on error) → `pending` (after backoff)
- `failed` × 5 consecutive → `shelved` (no auto-retry; notification fires; parent can clear via admin route)

There is no soft-shelve state in this design — `shelved` always means "needs explicit parent attention". (An earlier draft included a `'no_subscribers'` soft shelve; it was removed when the eligibility gate was simplified to allowlist-only.)

### `parent_notifications` CHECK extension — migration `021_…sql`

Migration 003 stripped `sync_error` from the allowed notification types. We need a new one. Per the SQLite convention used elsewhere in this codebase (table rebuild via `_new` + copy + drop, see `migrations/003_remove_sync_columns.sql`):

```sql
-- Rebuild parent_notifications with extended CHECK
CREATE TABLE parent_notifications_new (
    /* …same columns… */
    type TEXT NOT NULL CHECK (type IN (
        'ytdlp_failure', 'new_search_term', 'system_update',
        'channel_backfill_error'
    )),
    /* … */
);
INSERT INTO parent_notifications_new SELECT * FROM parent_notifications;
DROP TABLE parent_notifications;
ALTER TABLE parent_notifications_new RENAME TO parent_notifications;
-- Recreate indexes
```

And add `TYPE_CHANNEL_BACKFILL_ERROR = "channel_backfill_error"` in `src/services/notifications.rs:28-30`.

## Code: changes to `src/services/feed_cache.rs` and `src/services/feed_refresher.rs`

### `feed_cache.rs` — replace `feed_source_items` writes with `channel_videos` writes

- **Remove**: `PER_SOURCE_CAP` constant, `replace_source_items` function, the `ItemRow` struct's coupling to `feed_source_items`.
- **Add**:
  ```rust
  pub async fn upsert_channel_videos_from_rss(
      pool: &SqlitePool,
      channel_id: &str,
      items: &[ItemRow],
  ) -> AppResult<UpsertStats> { ... }

  pub async fn upsert_channel_videos_from_sidecar(
      pool: &SqlitePool,
      channel_id: &str,
      items: &[ItemRow],
  ) -> AppResult<UpsertStats> { ... }
  ```
  Both use the same SQL except for the `source` column value:
  ```sql
  INSERT INTO channel_videos
      (channel_id, video_id, title, channel_title, published_at, published_raw,
       thumbnail_url, first_seen_at, last_seen_at, source, is_deleted)
  VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?8, ?9, 0)
  ON CONFLICT(channel_id, video_id) DO UPDATE SET
      title         = excluded.title,
      channel_title = COALESCE(excluded.channel_title, channel_videos.channel_title),
      published_at  = COALESCE(channel_videos.published_at, excluded.published_at),
      published_raw = COALESCE(channel_videos.published_raw, excluded.published_raw),
      thumbnail_url = COALESCE(excluded.thumbnail_url, channel_videos.thumbnail_url),
      last_seen_at  = excluded.last_seen_at,
      source        = excluded.source,
      is_deleted    = 0;  -- re-sighting clears the tombstone
  ```
  Returns `UpsertStats { inserted, updated, untombstoned }` for diagnostics.

- **Rewrite `feed_for_child`** (`src/services/feed_cache.rs:465-541`): swap the `FROM feed_source_items fsi JOIN allowlisted_channels ac ON fsi.source_id = ac.channel_id` join over to `FROM channel_videos cv JOIN allowlisted_channels ac ON cv.channel_id = ac.channel_id`, add `WHERE cv.is_deleted = 0`, change the ordering tiebreaker from `fetched_at` to `last_seen_at`, and add a fresh `LIMIT` (currently implicit via the 20-cap; now make it explicit, e.g. `LIMIT 60` for the New Videos row). The home feed gets *strictly more* items available to surface than before; the LIMIT in the query controls how many we show.

### `feed_refresher.rs` — point writes at `channel_videos`

`poll_one` (`src/services/feed_refresher.rs:514-619`) currently calls `feed_cache::replace_source_items` on the RSS `Updated` outcome and the sidecar `Items` outcome. Replace those calls with `upsert_channel_videos_from_rss` / `upsert_channel_videos_from_sidecar`. Everything else in the refresher — RSS polling, ETag/Last-Modified handling, sidecar fallback rate caps, backoff, lease, jitter — is unchanged.

Notable removal: the `SIDECAR_FALLBACK_MAX_ITEMS = 15` truncation (`src/services/feed_refresher.rs:135`) becomes vestigial — there's no per-source cap to protect anymore. Keep it as a sanity ceiling against a runaway sidecar response.

### Child channel videos page — repoint to `channel_videos`

`GET /api/channels/:channelId/videos` (`src/routes/channels.rs:74-112::list_videos`) today calls `YoutubeClient::list_channel_videos`, which hits the discovery sidecar's `/channel-videos/:channelId` endpoint. The sidecar is single-page (`next_page_token` is always `null` — `sidecar/discovery/server.js:280-287`), so today's UI is capped at ~30 newest videos per channel with no pagination beyond that. Every page-open is a live InnerTube call.

Repoint this route at `channel_videos`:

```rust
// New body of list_videos (replacing the sidecar call at channels.rs:82-85)
// Pagination via offset cursor, matching the child_search pattern at search.rs:80-95.
let cursor = q.page_token.as_deref().and_then(decode_offset_token).unwrap_or(0);

let rows: Vec<ChannelVideoItem> = sqlx::query_as(
    "SELECT video_id, title, channel_id, channel_title, thumbnail_url,
            published_at, duration_s, view_count
     FROM channel_videos
     WHERE channel_id = ? AND is_deleted = 0
     ORDER BY
        CASE WHEN ? = 'most_viewed' THEN COALESCE(view_count, -1) ELSE 0 END DESC,
        published_at DESC,
        last_seen_at DESC
     LIMIT ? OFFSET ?"
).bind(&channel_id)
 .bind(q.sort.as_deref().unwrap_or("latest"))
 .bind(PAGE_SIZE as i64)
 .bind(cursor)
 .fetch_all(&state.db).await?;

// existing can_child_view filter applies unchanged

let next_page_token = if rows.len() as u32 >= PAGE_SIZE {
    Some(encode_offset_token(cursor + PAGE_SIZE as i64))
} else {
    None
};
```

Wins:
- **Zero YouTube calls** when a child opens the channel page (was 1 sidecar call per page-open).
- **Full archive pagination** (was capped at ~30 newest, hard).
- **`most_viewed` sort now actually works** for backfilled channels. The original code at `channels.rs:103-106` had to silently degrade to `latest` because the sidecar response didn't carry view counts. yt-dlp's flat-playlist populates `view_count` in `channel_videos`, so we can finally order by it. RSS-only rows (with NULL view_count) sort to the bottom of the most_viewed list via `COALESCE(view_count, -1)`.

Edge case — freshly-allowlisted channel within the brief window between allowlist POST and first backfill completion:
- ~30s post-add: RSS has landed → 15 newest videos visible (`source='rss'`).
- ~1h post-add (best case, single-channel install): backfill complete → full archive visible.
- If the user opens the channel page in the first ~30s, they may see 0 videos. This is rare, recoverable (refresh), and the same window also affects the New Videos feed today. Worth noting in the UI: an empty channel page with a "syncing…" state would be a nice frontend follow-up.

`enforce_channel_access` (`src/routes/channels.rs:122-151`) is unchanged.

### Channel header metadata route — also repointed

`GET /api/channels/:channelId` (`src/routes/channels.rs:33-46::get_channel`) today calls the sidecar `/channels/:id` and returns `ChannelInfo`. With the new columns on `channel_sync_state` (`channel_thumbnail_url`, `description`) plus the body-data forwarding path on allowlist POST (which carries `title`, `thumbnail`, and `description` from `SearchItem`), we can serve this route entirely from local state too.

```rust
// Replacement body of get_channel
let row = sqlx::query_as::<_, (String, Option<String>, Option<String>, Option<String>)>(
    "SELECT channel_id, channel_title, channel_thumbnail_url, description
     FROM channel_sync_state
     WHERE channel_id = ?"
).bind(&channel_id).fetch_optional(&state.db).await?
 .ok_or(AppError::NotFound)?;

// video_count is computed locally — more accurate than YouTube's number
// because it matches what the child actually sees (excludes tombstones).
let video_count: Option<i64> = sqlx::query_scalar(
    "SELECT COUNT(*) FROM channel_videos
     WHERE channel_id = ? AND is_deleted = 0"
).bind(&channel_id).fetch_one(&state.db).await.ok();

Ok(Json(ChannelInfo {
    id: row.0,
    title: row.1.unwrap_or_default(),
    description: row.3.unwrap_or_default(),
    thumbnails: thumbnail_map_from_url(row.2),  // single-entry HashMap keyed "default"
    video_count,
    // subscriber_count is dropped from ChannelInfo — see "Drop subscriber_count" below
}))
```

**Field-by-field source-of-truth:**

| Field | Source | Notes |
|---|---|---|
| `id` | `channel_sync_state.channel_id` | |
| `title` | `channel_sync_state.channel_title` | From `SearchItem` body-data, or sidecar fallback |
| `description` | `channel_sync_state.description` | From `SearchItem` body-data, or sidecar fallback |
| `thumbnails` | `channel_sync_state.channel_thumbnail_url` | Single URL wrapped in `HashMap<String, ThumbnailInfo>` for response compatibility |
| `video_count` | `COUNT(*) FROM channel_videos WHERE channel_id=? AND is_deleted=0` | Computed live; matches what the child sees, ignores YouTube's number |

**Effect on flow:** with this change, opening a child's channel page (header + video grid) makes **zero YouTube calls**. The full channel-browse experience is now local.

### Render channel description on the child channel page

The `description` field is plumbed end-to-end (forwarded from `SearchItem` via `AddChannelBody`, stored in `channel_sync_state.description`, served by `GET /api/channels/:channelId`) — but `frontend/src/components/channel-detail.ts` today does not render it. The parent preview UI (`preview-channel.ts:134-136`) does render it but uses a different endpoint. Adding a child-side render keeps the plumbing useful and gives the channel page a real header.

Add a description render to `channel-detail.ts`:

```html
<!-- After the existing title row in the header template -->
${info.description ? html`<p class="description">${info.description}</p>` : nothing}
```

Reuse the visual style established by `preview-channel.ts:51-55` (the existing `.description` CSS) for consistency. Estimated change: ~5 lines of TS + a CSS block (or a shared style import if the existing one can be lifted out).

This is the consumer that justifies carrying `description` through the new storage; without it, the field would be dead plumbing (which is why `subscriber_count` was dropped entirely — see next section).

### Drop `subscriber_count` entirely

YouTube's reported subscriber count provides no functional value in HomeTube (a kids' parental-controls app where rough fame indicators don't gate anything), and removing it removes both a schema column and a refresh-staleness problem.

Drop it from:

**Backend:**
- `ChannelInfo` struct (`src/services/youtube.rs:105`) — remove `pub subscriber_count: Option<i64>`. The sidecar JSON may still include it; serde will silently ignore the field with `#[serde(default)]`-style deserialisation already on the struct (no breakage).
- `ChannelPreview` (`src/routes/preview.rs:58-63`) — inherits the drop via `#[serde(flatten)] pub channel: ChannelInfo`. No standalone change needed; the field just disappears from the preview response.
- `channel_sync_state` schema — already omitted from the table definition above.
- `add_channel` handler — already simplified above (no `subscriber_count` capture).
- yt-dlp backfill — no `--print "%(channel_follower_count)s"` capture step. Plain `--flat-playlist` output is consumed and channel-level metadata other than title is not retained.

**Frontend:**
- `frontend/src/types/index.ts:207` — remove `subscriber_count: number | null` from `ChannelInfo`.
- `frontend/src/types/index.ts:349` — remove `subscriber_count: number | null` from `ChannelPreview`.
- `frontend/src/components/channel-detail.ts:183-187` — delete the conditional render of `${...toLocaleString()} subscribers`.
- `frontend/src/components/channel-detail.ts:61-64` — delete the now-unused `.stats` CSS class.
- `frontend/src/components/preview-channel.ts:129-133` — delete the equivalent render in the parent preview UI.
- `frontend/src/components/channel-card.ts` — already dead code (no template sets `subscriber-count`), but clean up: delete the docstring mention (line 4), `@property subscriberCount` (lines 26-27), `.subs` CSS (lines 84-87), `formatSubs(n)` helper (lines 96-100), and the conditional render (lines 111-113).

This is purely subtractive — no replacement field, no migration data loss (the column never existed; it would have been new in this plan). Net effect: less code, less UI, less data lag, zero functional impact.

### Child search — incidental but significant improvement

`src/routes/search.rs` (`child_search`, lines 138–255) is already entirely local; it never hits YouTube. One of its three video-source branches joins `feed_source_items` against `allowlisted_channels` (`search.rs:352-358`). With `feed_source_items` removed, that branch needs a swap:

```sql
-- before
SELECT fsi.video_id, fsi.title, fsi.channel_id, fsi.channel_title, fsi.thumbnail_url
FROM feed_source_items fsi
INNER JOIN allowlisted_channels ac
  ON ac.channel_id = fsi.source_id AND fsi.kind = 'channel'
WHERE ac.child_account_id = ?

-- after
SELECT cv.video_id, cv.title, cv.channel_id, cv.channel_title, cv.thumbnail_url
FROM channel_videos cv
INNER JOIN allowlisted_channels ac
  ON ac.channel_id = cv.channel_id
WHERE ac.child_account_id = ? AND cv.is_deleted = 0
```

The functional consequence is large: today's branch can only surface the ~20 newest uploads per allowlisted channel (the `PER_SOURCE_CAP` constraint on `feed_source_items`). After the swap, child search spans the **full archive** of every allowlisted channel — a child can finally find an older video by title if its channel is allowlisted, where today the query would silently miss it. No new code paths, no new YouTube calls; the breadth comes entirely from the unified storage.

Parent search (`GET /api/parent/search`, `src/routes/search.rs:48-57`) is untouched and continues to hit the discovery sidecar live — that's the discovery-for-allowlisting use case and live YouTube is the right backend for it.

### Admin diagnostics surface

`GET /api/admin/feed-sources` (`src/routes/feed.rs:519`) currently reports `item_count` from `feed_source_items`. Repoint it to `COUNT(*) FROM channel_videos WHERE channel_id = ? AND is_deleted = 0`. Add a parallel `archived_count` (`AND is_deleted = 1`) for completeness.

## Code: new module `src/services/channel_backfill.rs`

Modelled on `src/services/feed_refresher.rs` and `src/services/feed_cache.rs` (single file is fine; ~500 lines).

### Public API

```rust
pub fn spawn(pool: SqlitePool);                      // start the background loop
pub async fn enqueue(pool: &SqlitePool, channel_id: &str) -> AppResult<()>;
pub async fn reconcile_with_allowlist(pool: &SqlitePool) -> AppResult<()>;
pub async fn list_state(pool: &SqlitePool) -> AppResult<Vec<BackfillStateRow>>;
pub async fn unshelve(pool: &SqlitePool, channel_id: &str) -> AppResult<()>;
```

### Loop shape (`spawn`)

```
loop {
    sleep until backfill_next_at == min(t) OR idle_tick (60s)
    refresh tunables from app_config
    claim_one_due() (single-concurrency; this loop deliberately does not batch)
    match result {
        None => continue
        Some(row) => run_backfill_for(row).await
    }
    sleep min_gap_between_channels (default 1h; jittered ±15%)
}
```

Single-concurrency by design: we never have two yt-dlp `--flat-playlist` subprocesses inflight, ever. Across the family this is plenty — a 200-channel allowlist backfilled at 1 channel/hour completes its initial pass in ~8 days, then settles into monthly re-backfills.

### Re-backfill is rate-limited identically to initial backfill

There is no "fast path" for re-backfills. A row whose `backfill_next_at` fired due to the 30-day interval expiring re-enters the same `claim_one_due()` work queue as a brand-new pending row, observes the same `min_gap_between_channels` (1h, jittered) gate, and runs through the same `run_backfill_for` function spawning the same yt-dlp `--flat-playlist` subprocess with the same flags.

**Emergent property — no thundering herd at 30-day mark.** Because the initial backfill wave processed channels serially at ≥1h apart, the assigned `backfill_next_at = completion_time + 30d` values are also ≥1h apart by construction. The natural pacing of the initial pass carries through into the re-backfill cycle without explicit coordination.

**Belt-and-braces — re-backfill interval jitter.** As a safety net against synchronisation from out-of-band triggers (admin `run-now`, late bulk-adds, restored-from-backup state), apply ±5% jitter to the re-backfill interval at completion time:

```rust
// In mark_complete (channel_backfill.rs)
let interval = re_backfill_interval_s;
let jitter_range = (interval as f64 * 0.05) as i64;
let jittered = interval as i64 + rand::thread_rng().gen_range(-jitter_range..=jitter_range);
let next_at = now + jittered;
```

±5% on 30 days is ~36h of smear — easily disrupts any accidental synchronisation while keeping the cadence predictable for diagnostics.

### Anti-bot pacing tunables (all in `app_config`, all live-reload)

| Key | Default | Range | Notes |
|---|---|---|---|
| `channel_backfill.enabled` | `true` | bool | kill switch |
| `channel_backfill.min_gap_between_channels_s` | `3600` | 300..=86400 | jittered ±15% |
| `channel_backfill.re_backfill_interval_s` | `2592000` (30d) | 86400..=31536000 | once-complete pacing |
| `channel_backfill.subprocess_timeout_s` | `1800` | 60..=14400 | per-channel ceiling |
| `channel_backfill.ytdlp_sleep_requests_s` | `1` | 0..=10 | passed to yt-dlp `--sleep-requests` |
| `channel_backfill.ytdlp_sleep_interval_s` | `1` | 0..=10 | `--sleep-interval` |
| `channel_backfill.ytdlp_max_sleep_interval_s` | `3` | 0..=30 | `--max-sleep-interval` |
| `channel_backfill.max_consecutive_errors_before_shelve` | `5` | 1..=20 |  |
| `channel_backfill.notify_on_shelve` | `true` | bool | parent notification toggle |

Two key things compared to `feed_refresher`:
1. The pacing is *per-channel* not per-request — one channel/hour, no concurrent dispatches.
2. We let yt-dlp itself add intra-channel sleeps (`--sleep-requests`/`--sleep-interval`) so InnerTube pagination requests are spaced out inside one subprocess.

### The work function

```rust
async fn run_backfill_for(pool, channel_id) -> Result<RunStats> {
    let started_at = now_unix();
    mark_running(channel_id, started_at, lease=started_at+subprocess_timeout+300)
    let stub = spawn_ytdlp_flat_playlist(channel_id, tunables).await?;
    // stub is a stream of yt-dlp JSON lines

    let mut tx = pool.begin().await?;
    let mut observed = HashSet::new();
    for line in stub.lines {
        let v = parse_flat_playlist_entry(line)?;
        // Upsert with source='backfill', last_seen_at=started_at, is_deleted=0.
        // Populate duration_s/view_count if yt-dlp supplied them.
        upsert_channel_video_from_backfill(&mut tx, &v, started_at).await?;
        observed.insert(v.video_id);
    }
    // Reconcile is_deleted: only rows that pre-date this run AND weren't seen become tombstones.
    // This guards against tombstoning RSS-fed rows that arrived during the backfill window.
    sqlx::query(
        "UPDATE channel_videos
         SET is_deleted = 1
         WHERE channel_id = ?1
           AND is_deleted = 0
           AND first_seen_at < ?2
           AND video_id NOT IN (SELECT value FROM json_each(?3))"
    ).bind(channel_id).bind(started_at).bind(json!(observed)).execute(&mut *tx).await?;
    tx.commit().await?;

    mark_complete(channel_id, stats).await?;
    Ok(stats)
}
```

Key correctness points:
- **`source='backfill'` on upsert** is informational but lets diagnostics distinguish "rows we've fully verified" from "rows RSS told us about but backfill hasn't seen yet".
- **`first_seen_at < started_at` in the reconciliation clause** is the critical safety property — without it, an RSS upsert that happens mid-backfill would get tombstoned at commit. With it, brand-new rows get a free pass until the *next* backfill cycle confirms them.
- The transaction wraps the whole pass so a subprocess crash mid-stream leaves the DB unchanged. For very large channels (10k+ uploads) this is fine in SQLite WAL mode.

### yt-dlp invocation

A new helper in `src/services/ytdlp.rs` (or a thin wrapper around an existing one):

```rust
pub async fn flat_playlist_channel(
    channel_id: &str,
    tunables: &BackfillTunables,
) -> AppResult<impl Stream<Item = AppResult<FlatPlaylistEntry>>>;
```

It mirrors the cookie/PO-token/plugin-dir setup already present in `src/services/ytdlp.rs:510-731` (cookies copied to per-invocation tempfile to preserve the canonical jar; bgutil plugin dir + `POT_SERVER_URL`). Differences from the existing `--dump-json` per-video path:
- `--flat-playlist --skip-download`
- streams JSON lines via `BufReader::lines` rather than parsing one blob
- 30 min default timeout (vs 30 s for single-video)
- additional `--extractor-args "youtubetab:approximate_date"` so we get *some* `upload_date` even for channels with sparse metadata
- URL: `https://www.youtube.com/channel/<channel_id>/videos` (the `/videos` tab — uploads only, excluding shorts/lives by default)

The `/videos` tab is the only one consumed. HomeTube doesn't support shorts or live streams as playable content types, so backfilling them would create rows the rest of the system can't render or play.

### Failure handling

Classify yt-dlp stderr with the same patterns the discovery sidecar uses (`sidecar/discovery/server.js:362-411`) — `sign in`, `consent`, `429`, `rate-limit`, `403`. On match: record failure, exponential backoff (base 1h, ×2, cap 24h), don't shelve until 5 consecutive. On 5 consecutive: set `status='shelved'`, dispatch `channel_backfill_error` notification with `dispatch_ytdlp_failure_deduped`-style 24h dedup (`src/services/notifications.rs:248`).

Channel-not-found (HTTP 404 / "This channel does not exist"): single-strike shelve with `last_error="channel_not_found"`, no parent notification (probably a deleted/renamed channel — surface in admin diagnostics only).

## Lifecycle wiring

### Startup reconciliation — `src/main.rs`

Replace the existing `backfill_feed_sources` function call (`src/main.rs:88-99`, function body at `:163-176`) — which only seeded `feed_sources` from `allowlisted_channels` — with the new combined reconcile. SQL is in the "Eligibility lifecycle" section.

```rust
// Seed channel_sync_state for every allowlisted channel; GC orphans.
// Replaces the old backfill_feed_sources helper.
channel_backfill::reconcile_with_allowlist(&pool).await?;

// Spawn the freshness loop (existing module; reads channel_sync_state instead of feed_sources).
feed_refresher::spawn(pool.clone());

// Spawn the channel-history backfiller (new).
channel_backfill::spawn(pool.clone());
```

The `backfill_feed_sources` helper at `src/main.rs:163-176` is deleted.

### Live add — `src/routes/allowlist.rs` and `src/routes/subscriptions.rs`

- Allowlist add (`POST /api/children/:id/allowlist/channels`): seed `channel_sync_state` unconditionally. SQL in the "Allowlist-route wiring" section.
- Subscribe (`POST /api/subscriptions`): priority hint only — bump `backfill_next_at=0` iff the channel is pending and never backfilled. SQL in the "Subscription-route wiring" section.

### Live remove — `src/routes/allowlist.rs`

- Un-allowlist (`DELETE /api/children/:id/allowlist/channels/:channelId`, `src/routes/allowlist.rs:116-146`): when no child still has it allowlisted, GC the `channel_sync_state` row and any `channel_videos` for that channel.
- Unsubscribe (`DELETE /api/subscriptions/:channelId`): **no changes needed**. Subscriptions are decoupled from sync eligibility.

The `feed_gc` cron (`src/services/cron.rs:49`) also calls `channel_backfill::reconcile_with_allowlist` daily so any allowlist churn that didn't hit the route wiring eventually reconciles.

## Admin / observability surfaces

### New routes under `src/routes/feed.rs` (or new `src/routes/channel_backfill.rs`)

Mirror the existing `/api/admin/feed-sources` / `/api/admin/feed-refresher/{settings,capacity}` shape:

| Method | Path | Body / Use |
|---|---|---|
| `GET` | `/api/admin/channel-backfill/state` | list rows from `channel_sync_state` joined with `COUNT(*)` from `channel_videos`; surface `backfill_status`, `backfill_last_error`, `backfill_consecutive_errors`, totals. |
| `GET` | `/api/admin/channel-backfill/settings` | effective vs raw tunables; identical pattern to `feed_refresher` settings route at `src/routes/feed.rs:329`. |
| `PUT` | `/api/admin/channel-backfill/settings` | validated writes to `app_config` (`RANGE_…` consts). |
| `POST` | `/api/admin/channel-backfill/run-now/:channelId` | set `backfill_next_at=0`; loop picks it up within `idle_tick`. |
| `POST` | `/api/admin/channel-backfill/unshelve/:channelId` | clear `backfill_status='shelved'` → `pending`, reset `backfill_consecutive_errors`, `backfill_next_at=0`. |

All admin routes parent-gated, same auth pattern as existing `/api/admin/*` (`src/routes/feed.rs`). Consumed by the new `<hometube-channel-backfill-settings>` Lit component mounted on the existing `/parent/system` page (see "Parent UI: channel backfill settings + per-channel state" section under "## Rollout plan").

(No separate `videos-archive` endpoint is needed — the child-facing `GET /api/channels/:channelId/videos` is repointed to `channel_videos` in this plan and already serves the full paginated archive with allowlist filtering applied.)

### Notifications

- New type `channel_backfill_error` (see migration 021 + `notifications.rs`).
- Helper `dispatch_channel_backfill_error_deduped(pool, channel_id, error)` modelled on `dispatch_ytdlp_failure_deduped` (`src/services/notifications.rs:248`) with 24 h dedup keyed by `channel_id`.
- Fires only on **shelve** (5 consecutive failures), not on every failure, to avoid notification storms.
- External push forwarders (ntfy/Gotify/Apprise) inherit automatically through `notifications::dispatch`/`broadcast`. Add `channel_backfill_error` to the priority maps at `src/services/notification_forwarders.rs:405,416` (medium priority).

## Anti-bot strategy — summary of safeguards

1. **Single-concurrency** background loop. At most one outstanding yt-dlp `--flat-playlist` family-wide.
2. **One channel per hour** by default, jittered ±15%, configurable.
3. **Intra-channel sleeps** via `--sleep-requests 1 --sleep-interval 1 --max-sleep-interval 3` so InnerTube pagination is spaced inside the subprocess.
4. **PO token + cookies** through the same `pot-server` and cookie-jar pipeline that already keeps the `/api/proxy/segment` extraction path alive.
5. **No periodic re-backfill** for 30 days after a complete pass.
6. **Bot-check signature detection** (sign-in / consent / 429 / 403) classifies failures and triggers exponential backoff with cap 24 h, not retry-immediately.
7. **Shared cooldown read** with `channel_sync_state.sidecar_last_fallback_at`: if the refresher recently took a sidecar fallback for a given channel, defer that channel's next backfill by an additional hour. This avoids stacking two anti-bot-sensitive operations on the same channel back-to-back.
8. **Default-on at first ship.** `channel_backfill.enabled = true` by default — see Resolved Decision #1. The kill switch is the `channel_backfill.enabled` tunable in `app_config`; flipping it to `false` halts the loop within `idle_tick` (60s) without restart.

## Testing

### Unit tests for `channel_backfill::run_backfill_for`

Against a fake `flat_playlist_channel` that yields a canned `Vec<FlatPlaylistEntry>`:
- First run: all rows inserted with `first_seen_at == last_seen_at == started_at`, `source='backfill'`, `is_deleted=0`.
- Second run with same items: `last_seen_at` bumped, `first_seen_at` preserved.
- Second run with a missing item: that row gets `is_deleted=1`, others untouched.
- Second run that adds a new item: only that row has `first_seen_at == now`.
- **Inter-tier interaction**: row inserted by RSS with `first_seen_at = T1` mid-backfill (where backfill `started_at = T0` and `T1 > T0`); even though backfill doesn't witness it, the reconciliation UPDATE *does not* tombstone it because `first_seen_at < started_at` is false. The next backfill (whose `started_at > T1`) will then either confirm or tombstone it.
- **Tombstone clearing**: row with `is_deleted=1` from a prior backfill is reset to `is_deleted=0` if a later RSS poll sights it (channel re-published a previously-deleted video).

### Unit tests for `feed_cache::upsert_channel_videos_from_rss`

- New row → inserted with `source='rss'`, `first_seen_at == last_seen_at`, `is_deleted=0`.
- Existing row from backfill → `last_seen_at` bumped, `source` becomes `'rss'`, `duration_s`/`view_count` preserved (not nulled).
- Existing row with `is_deleted=1` → tombstone cleared, `source='rss'`.
- RSS does NOT trigger any DELETE or `is_deleted=1` set — verified by a test where RSS returns 0 items.

### Unit tests for `feed_cache::feed_for_child` (New Videos)

- Query excludes `is_deleted=1` rows.
- Query orders by `published_at DESC, last_seen_at DESC`.
- Query respects allowlist + blocked + hidden filters as before (regression against the existing test suite for `feed_for_child`).

### Other tests
- Failure classification (sign-in / 429 / consent / 404 → expected enum variant).
- Backoff math + jitter bounds.
- Eligibility transitions in `reconcile_with_allowlist`: seed-on-new-allowlist, GC-on-last-unallowlist.
- Subscription priority-hint: subscribing a `pending`+never-completed channel sets `backfill_next_at=0`; subscribing an already-`complete` channel is a no-op.

### Integration tests
- Real `tests/` harness (sqlite-in-memory, mocked yt-dlp via a shim binary on `PATH` that emits canned JSON lines).
- Migration test: starting from a DB with `feed_source_items` rows, run migration 020, assert rows survive in `channel_videos`.

**No live YouTube traffic in CI.**

## Rollout plan

Phase 1 (this plan):
- Migration 020: create `channel_videos`; create `channel_sync_state` and migrate rows from `feed_sources`; drop `feed_sources`; copy `feed_source_items` rows into `channel_videos`; drop `feed_source_items`.
- Migration 021: extend `parent_notifications` CHECK with `channel_backfill_error`.
- Update `feed_cache.rs`: drop `replace_source_items`/`PER_SOURCE_CAP`, add `upsert_channel_videos_from_{rss,sidecar}`, rewrite `feed_for_child` against `channel_videos`.
- Update `feed_refresher.rs`: point write calls at the new upsert functions.
- Update `src/routes/search.rs::search_videos`: swap the third UNION branch from `feed_source_items` to `channel_videos` (one-line query change; semantically broadens child search from "20 newest per channel" to "full channel archive").
- Update `src/routes/channels.rs::list_videos`: replace the sidecar `list_channel_videos` call with a paginated read from `channel_videos`; switch to offset-based page tokens (matches `child_search` pattern); enable `most_viewed` sort against the now-populated `view_count` column. Removes 1 sidecar call per channel page-open and lifts the ~30-newest cap to full-archive pagination.
- Update `src/routes/channels.rs::get_channel`: replace the sidecar `/channels/:id` call with a local read from `channel_sync_state` + a `COUNT(*)` against `channel_videos` for `video_count`. Together with the videos-list repoint, brings the full channel-browse experience to zero YouTube calls per page-open.
- Remove `subscriber_count` from `ChannelInfo` (`src/services/youtube.rs:105`). `ChannelPreview` inherits the drop via `#[serde(flatten)]`. No yt-dlp capture step is added for subscriber count.
- Update `src/routes/allowlist.rs::add_channel` + `AddChannelBody` to accept optional `channel_title` / `channel_thumbnail_url` / `description` and treat the sidecar call as a best-effort fallback (mirrors the existing `add_video` pattern, eliminates the burst risk in the dominant flow). Backend rolls out backward-compatibly — old frontends still work, they just incur the sidecar call.

**Frontend changes (this plan):**

- `frontend/src/components/allowlist-manager.ts:262-270` — expand the channel branch of `addItemForKind` to forward `channel_title: item.title`, `channel_thumbnail_url: pickThumbnail(item.thumbnails)`, `description: item.description` in the POST body. Mirrors the video branch (lines 265-270). Note: use `item.title` (not `item.channel_title`) for the channel name on channel-kind results.
- `frontend/src/components/allowlist-manager.test.ts:113-127` (and/or `175-213`) — extend existing channel-POST test to assert the new body fields.
- `frontend/src/components/channel-detail.ts:183-187` — delete the conditional render of `${...toLocaleString()} subscribers`. Also delete the now-unused `.stats` CSS at lines 61-64.
- `frontend/src/components/channel-detail.ts` (header render) — **add** a description render below the title row: `${info.description ? html\`<p class="description">${info.description}</p>\` : nothing}`. Reuse the `.description` CSS style from `preview-channel.ts:51-55` (consider lifting it into a shared styles module). Consumes the description field plumbed through `AddChannelBody` → `channel_sync_state.description` → `GET /api/channels/:channelId`.
- `frontend/src/components/preview-channel.ts:129-133` — delete the subscriber-count render block in the parent preview UI (parallel surface; ChannelPreview drops the field via flattened ChannelInfo).
- `frontend/src/components/channel-card.ts` — delete dead subscriber-count code (already not set by any template): docstring line 4, `@property subscriberCount` lines 26-27, `.subs` CSS lines 84-87, `formatSubs(n)` helper lines 96-100, render block lines 111-113.
- `frontend/src/types/index.ts:207` — remove `subscriber_count` field from `ChannelInfo`. `frontend/src/types/index.ts:349` — remove `subscriber_count` field from `ChannelPreview`.
- New `channel_backfill.rs` module + `flat_playlist_channel` helper in `ytdlp.rs`.
- Startup reconcile + allowlist + subscription route wiring (per "Eligibility lifecycle").
- Admin routes for state / settings / run-now / unshelve; repoint `/api/admin/feed-sources` `item_count` at `channel_videos`.
- Default `channel_backfill.enabled=true`; backfilling kicks off the next time the app restarts on existing installs (reconcile populates state).
- **Parent UI for channel backfill settings + per-channel state** — new Lit component `frontend/src/components/channel-backfill-settings.ts` (twin of `feed-refresher-settings.ts`), mounted in `templates/pages/parent/system.html`. Per-row "Run now" / "Unshelve" buttons + tunables editor. Vitest covering GET/PUT round-trips and action wiring. See "Parent UI: channel backfill settings + per-channel state" section below.
- **Adaptive InnerTube sidecar fallback cadence** — replace fixed 1h-per-source min interval in `feed_refresher.rs` with a recency-bucketed lookup against `channel_videos`. 1h (active) / 6h (30-90d dormant) / 24h (>90d dormant). Five new tunables in `app_config` exposed via the existing feed-refresher settings route. No new schema. See "Adaptive InnerTube sidecar fallback cadence" section below.
- **Thumbnail prefetching to a new on-disk cache** — net-new `src/services/thumbnail_store.rs` (parallel to `segment_store.rs`), migration 022 (`thumbnail_cache` table + `app_config` size cap), proxy route at `src/routes/videos.rs:903-932` updated to read disk-first, backfill loop tail-call enqueues prefetch for newly-observed videos, cache cleanup folded into existing `cache_cleanup` cron, parent cache-manager UI extended to surface the second cache. Largest of the three promoted items. See "Thumbnail prefetching — net-new on-disk thumbnail cache" section below.
- Migration 022: create `thumbnail_cache` table (parallel to `segment_cache`). Add `thumbnail_cache.max_bytes` to `app_config`.

**Incidental cleanup (this plan):**

- Delete `src/db/queries.rs`. It is a 4-line stub (`//! Database queries.` docstring + a comment noting it's "Currently empty — Phase 1 only provisions the database; later phases populate it") that has been empty since the original 20-phase plan was abandoned in favour of inlining SQL into route/service modules. The convention this plan continues (SQL co-located with the module that owns the operation: `feed_cache.rs`, the new `channel_backfill.rs`, `src/routes/allowlist.rs`, etc.) is now well-established across the codebase, so the stub no longer represents a pending migration target. Remove the file and drop the `pub mod queries;` line from `src/db/mod.rs`. No callers to update — verified empty in initial explore.

### Parent UI: channel backfill settings + per-channel state

The existing parent system page (`templates/pages/parent/system.html:35-38`) already hosts `<hometube-feed-refresher-settings>` — a parallel component for the existing freshness refresher with tunables editor + diagnostics. The channel backfill subsystem gets its own twin:

**New Lit component**: `frontend/src/components/channel-backfill-settings.ts`

Modelled on `frontend/src/components/feed-refresher-settings.ts`. Composition:

- **Settings editor section** — reads from `GET /api/admin/channel-backfill/settings`, writes to `PUT /api/admin/channel-backfill/settings`. Exposes the tunables: `enabled`, `min_gap_between_channels_s`, `re_backfill_interval_s`, `subprocess_timeout_s`, `ytdlp_sleep_*` flags, `max_consecutive_errors_before_shelve`, `notify_on_shelve`.
- **Per-channel state table** — reads from `GET /api/admin/channel-backfill/state`. Columns: channel title, status (with colour-coded pill), last completed at, last error (truncated), consecutive errors, videos observed/new/removed (last run), next due at. Per-row action buttons:
  - **"Run now"** → `POST /api/admin/channel-backfill/run-now/:channelId`. Sets `backfill_next_at=0`; loop picks up within `idle_tick`. Confirmation toast on success.
  - **"Unshelve"** → `POST /api/admin/channel-backfill/unshelve/:channelId`. Only visible when `backfill_status='shelved'`. Resets `backfill_status='pending'`, `backfill_consecutive_errors=0`, `backfill_next_at=0`.
- **Capacity / health summary** at the top — total channels, complete count, pending count, shelved count, average days-since-last-completed. Pulled from the same `/api/admin/channel-backfill/state` payload (computed server-side or client-side aggregate).

**Mount in `templates/pages/parent/system.html`**: insert a new `<div class="page-section">` between the existing "New-videos refresher" and "Notifications" sections, and add the `<script type="module" src="/assets/components/channel-backfill-settings.js">` import block.

**Tests**: new `frontend/src/components/channel-backfill-settings.test.ts` modelled on `feed-refresher-settings.test.ts` — vitest with mocked admin endpoints, asserting GET/PUT round-trips and per-row action wiring.

### Adaptive InnerTube sidecar fallback cadence

RSS itself has no anti-bot exposure so its cadence is left alone. But the sidecar fallback (the anti-bot-sensitive freshness path) currently uses a fixed `SIDECAR_FALLBACK_MIN_INTERVAL_S=3600` per source regardless of how active the channel is. Scale this per-channel based on observed publishing recency:

| Channel activity | Per-source min interval |
|---|---|
| Most recent upload ≤ 30 days ago | 1 h (unchanged from today) |
| Most recent upload 30–90 days ago | 6 h |
| Most recent upload > 90 days ago (or no uploads ever observed) | 24 h |

A channel dormant for 90+ days has already gone weeks without anything new — a 24h gap on the riskier transport costs the family essentially nothing in freshness and meaningfully reduces sidecar load for the long tail of allowlisted-but-dormant channels (e.g. defunct kids' shows, completed series).

**Implementation** in `src/services/feed_refresher.rs`:

- Add a helper `effective_sidecar_min_interval(pool, channel_id) -> i64` that queries:
  ```sql
  SELECT MAX(published_at) FROM channel_videos
  WHERE channel_id = ? AND is_deleted = 0
  ```
  and buckets the result against `now` into the three tiers. NULL (no rows) is treated as ">90 days" (24h).
- Replace the current call site that reads `SIDECAR_FALLBACK_MIN_INTERVAL_S` (line ~109) with a call to the helper, gated on the existing `last_sidecar_fallback_at` check.
- The aggregate cap `SIDECAR_FALLBACK_MAX_PER_HOUR=120` stays unchanged; it's a separate dimension.

**New tunables** in `app_config` (live-reloaded like the existing ones, range-validated per the precedent at `RANGE_…` consts in feed_refresher.rs):

| Key | Default | Range |
|---|---|---|
| `feed_refresher.sidecar_fallback_active_interval_s` | `3600` | 60..=86400 |
| `feed_refresher.sidecar_fallback_dormant_interval_s` | `21600` | 60..=86400 |
| `feed_refresher.sidecar_fallback_archived_interval_s` | `86400` | 60..=604800 |
| `feed_refresher.sidecar_dormant_threshold_days` | `30` | 1..=365 |
| `feed_refresher.sidecar_archived_threshold_days` | `90` | 1..=3650 |

Surfaced via the existing `GET /PUT /api/admin/feed-refresher/settings` route alongside the current tunables.

**No new column on `channel_sync_state`** — recency is computed on-demand from `channel_videos`. The cost is one indexed query per RSS failure event (which is uncommon by definition — most RSS polls are 304s or 200s). The query hits `idx_channel_videos_channel_published`.

### Thumbnail prefetching — net-new on-disk thumbnail cache

The original Phase 2 phrasing assumed an existing thumbnail cache to prefetch into. There isn't one — `GET /api/proxy/thumbnail/:videoId` (`src/routes/videos.rs:903-932`) reverse-proxies every request to `i.ytimg.com/vi/<id>/...` with no caching layer. The "on-disk cache" called out in the README refers to DASH video segments only (`src/services/segment_store.rs`).

To prefetch thumbnails meaningfully we need to add a parallel cache. Scope:

**New module**: `src/services/thumbnail_store.rs`, modelled on `src/services/segment_store.rs`. Provides:

- `get(video_id) -> Option<Vec<u8>>` — read cached bytes if present.
- `put(video_id, bytes)` — write to disk, update DB index.
- LRU eviction with the parent-tunable size cap pattern already used for segments.

**New migration `022_thumbnail_cache.sql`**: a `thumbnail_cache` table parallel to the existing `segment_cache` schema (`migrations/006_segment_cache_total_bytes.sql`, `migrations/007_cache_evictions.sql` model) — columns `(video_id PRIMARY KEY, byte_size, fetched_at, last_accessed_at)`. Plus an `app_config` entry for the size cap (e.g. `thumbnail_cache.max_bytes`, default 500 MB).

**Route update**: `GET /api/proxy/thumbnail/:videoId` (`src/routes/videos.rs:903-932`) reads `thumbnail_store::get` first; on miss, fetches from `i.ytimg.com`, stores, and serves.

**Prefetch trigger**: piggyback on the backfill loop. After `run_backfill_for` finishes its upsert pass, enqueue an async prefetch task for any video rows where `first_seen_at == last_seen_at` (i.e. newly observed this pass). Prefetcher fetches `https://i.ytimg.com/vi/<video_id>/hqdefault.jpg` (with `mqdefault.jpg` fallback on 404) at a slow rate — e.g. 1 image/sec — and writes through `thumbnail_store::put`. Bounded by an in-process semaphore (e.g. 2 concurrent fetches). Failures are silent — the proxy route still works on cache miss.

**Cache cleanup** is folded into the existing `cache_cleanup` cron job (`src/services/cron.rs:48`) — same LRU eviction policy already used for `segment_cache`. Add a parallel `thumbnail_cache_cleanup` call in the same cron handler.

**Admin surface** in the existing parent cache-manager UI (`frontend/src/components/cache-manager.ts`): add a second cache section showing thumbnail cache size + max + eviction count, mirroring the segment cache display. The component already follows this pattern for segment cache.

**Effective behavior**: after a channel is backfilled, its thumbnails trickle into the local disk cache over the following minutes. Subsequent child renders of the channel page or New Videos feed serve thumbnails from disk with no YouTube hit. Cold-start (never-backfilled-yet channel) still hits YouTube on demand.

**Scope honesty**: this is the largest of the three promoted items — net-new schema, new module, new background task, route change, UI change. It's still well-bounded (clear precedent in the segment cache implementation), but if Phase 1 needs to ship sooner, this is the cleanest item to break out into a follow-up plan.

Items removed from Phase 2 in this revision:
- "Browse all videos from this channel" child UI — already covered by the Phase 1 repoint of `src/routes/channels.rs::list_videos` to read from `channel_videos` with full pagination. The existing child channel page (`templates/pages/child/channel.html`) is the consumer; no new UI needed.
- "Optional `/shorts` and `/streams` tabs in backfill" — HomeTube doesn't support shorts or live streams as content types, so backfilling them would yield rows that the rest of the system can't render or play.

## Resolved decisions

1. **Default on.** `channel_backfill.enabled` defaults to `true`. On first start after upgrade, `reconcile_with_allowlist` seeds state for every allowlisted channel and the loop begins at the configured cadence.
2. **Family-global cadence.** `channel_backfill.re_backfill_interval_s` is a single tunable applied to every channel. Per-channel cadence is explicitly *not* added in v1. Reasoning: the loop is already strictly serial, so "faster" per-channel cadence cannot speed anything up — it can only displace other channels and increase aggregate YouTube footprint. A single global knob also keeps anti-bot budgeting predictable and removes a footgun (a parent setting "daily" on a heavy channel quietly doubles or triples that channel's YouTube load). If a parent wants a specific channel re-backfilled immediately, `POST /api/admin/channel-backfill/run-now/:channelId` exists.
3. **Keep `is_deleted` rows forever.** No periodic GC of tombstoned video rows. They remain joinable from watch history and surface in the archive read endpoint with `is_deleted=1` for the UI to render as "no longer available" if desired.
4. **Allowlist-only gate; subscriptions are a priority hint.** A channel is eligible for backfill as soon as it appears in `allowlisted_channels`. Subscriptions do not gate backfill (because the loop's 1-channel/hour cap protects YouTube regardless of queue depth, and search/discovery benefits from a pre-populated archive). However, when a child subscribes to an allowlisted channel that has never completed a backfill (`backfill_status='pending'` AND `backfill_last_completed_at IS NULL`), the subscribe-route hook bumps `backfill_next_at=0` so it jumps the queue. See the "Eligibility lifecycle" section below.

## Eligibility lifecycle

Eligibility predicate is simply: **the channel appears in `allowlisted_channels`** (deduped across children).

### Lifecycle table

| Event | Action |
|---|---|
| Channel allowlisted (`POST /api/children/:id/allowlist/channels`) | Seed `channel_sync_state(channel_id, backfill_status='pending', backfill_next_at=0, rss_next_poll_at=0)`. Both sync tiers will pick it up on the next loop tick. |
| Child subscribes to a channel (`POST /api/subscriptions`) | **Priority hint only.** If a `channel_sync_state` row exists with `backfill_status='pending'` AND `backfill_last_completed_at IS NULL`, set `backfill_next_at=0` so it jumps the queue. Otherwise no-op. Never *creates* a `channel_sync_state` row (subscriptions to non-allowlisted channels are still ignored for sync purposes). |
| Child unsubscribes (`DELETE /api/subscriptions/:channelId`) | **No effect on backfill.** Subscriptions are decoupled from sync eligibility. |
| Channel un-allowlisted (`DELETE /api/children/:id/allowlist/channels/:channelId`) | If no other child still has it allowlisted, GC the `channel_sync_state` row and any `channel_videos` for that channel. The existing rule in `src/routes/allowlist.rs:122-143` is the model. |

This is a strictly simpler lifecycle than the earlier subscribed-only design: no shelve/un-shelve on subscription churn, no `'no_subscribers'` soft-shelve state, no need to track "last active subscriber" on the unsubscribe path. The only soft-shelve state remaining is the error-driven one (5 consecutive backfill failures → `'shelved'`, requires parent un-shelve).

### `reconcile_with_allowlist`

```sql
-- 1. Seed rows for any newly-allowlisted channel
INSERT INTO channel_sync_state
    (channel_id, backfill_status, backfill_next_at, rss_next_poll_at)
SELECT DISTINCT channel_id, 'pending', 0, 0
FROM allowlisted_channels
ON CONFLICT(channel_id) DO NOTHING;

-- 2. GC channels no longer allowlisted by anyone
DELETE FROM channel_sync_state WHERE channel_id NOT IN (
    SELECT DISTINCT channel_id FROM allowlisted_channels
);
DELETE FROM channel_videos WHERE channel_id NOT IN (
    SELECT DISTINCT channel_id FROM allowlisted_channels
);
```

Runs at startup *and* from the `feed_gc` daily cron so allowlist churn during the day eventually reconciles without requiring a process restart.

### Allowlist-route wiring (`src/routes/allowlist.rs`)

#### Burst-safety: take channel metadata from the request body, not the sidecar

The current `add_channel` handler (`src/routes/allowlist.rs:64-108`) does an unconditional, hard-required sidecar `/channels/:id` call to resolve title and thumbnail. The video endpoint (`add_video`, lines 210–onwards) already does this differently: its `AddVideoBody` (lines 175–184) takes optional `title`/`channel_title`/`thumbnail_url` from the request body, and the handler treats the sidecar call as best-effort with body data as fallback. The doc comment is explicit that the UI has these fields from `/api/parent/search` results.

The channel endpoint should adopt the same pattern, with a stronger preference for body data. Extend `AddChannelBody`:

```rust
#[derive(Debug, Deserialize)]
pub struct AddChannelBody {
    pub channel_id: String,
    #[serde(default)]
    pub channel_title: Option<String>,
    #[serde(default)]
    pub channel_thumbnail_url: Option<String>,
    #[serde(default)]
    pub description: Option<String>,           // forward from SearchItem.description
}
```

`description` is also present on `SearchItem` (`src/services/youtube.rs:82-94`), so the parent search response already carries it. The frontend just needs to forward all three optional fields.

**Important nuance for the frontend forwarder:** for **channel-kind** `SearchItem` results, the channel's own title sits in `SearchItem.title`, not `SearchItem.channel_title` (the latter is the *owning* channel of a video for video-kind results). So the body builder must use `item.title`, not `item.channel_title`. See the frontend section below.

And update `add_channel` handler logic:

```rust
// 1. Try body data first (trim + filter empty per add_video convention).
let body_title = body.channel_title.as_deref().map(str::trim).filter(|s| !s.is_empty());
let body_thumb = body.channel_thumbnail_url.as_deref().map(str::trim).filter(|s| !s.is_empty());
let body_desc  = body.description.as_deref().map(str::trim).filter(|s| !s.is_empty());

// 2. Only call the sidecar if essential body data is missing.
//    Title is the gate — if present, we trust the rest of the body too.
let info = if body_title.is_some() {
    None
} else {
    YoutubeClient::from_db(&state.db).await?
        .get_channel(&body.channel_id).await.ok().flatten()
};

// 3. Combine, preferring body, then sidecar, then error if both empty.
let title = body_title.map(str::to_string)
    .or_else(|| info.as_ref().map(|i| i.title.trim().to_string()).filter(|s| !s.is_empty()))
    .ok_or_else(|| AppError::BadRequest("channel_title required (sidecar lookup also failed)".into()))?;
let thumb = body_thumb.map(str::to_string)
    .or_else(|| info.as_ref().and_then(|i| preferred_thumbnail(&i.thumbnails)));
let description = body_desc.map(str::to_string)
    .or_else(|| info.as_ref().map(|i| i.description.trim().to_string()).filter(|s| !s.is_empty()));

// 4. INSERT into allowlisted_channels (existing SQL, unchanged) ...

// 5. Seed channel_sync_state with the header metadata.
sqlx::query(
    "INSERT INTO channel_sync_state
         (channel_id, channel_title, channel_thumbnail_url, description,
          backfill_status, backfill_next_at, rss_next_poll_at)
     VALUES (?1, ?2, ?3, ?4, 'pending', 0, 0)
     ON CONFLICT(channel_id) DO UPDATE SET
         channel_title         = COALESCE(excluded.channel_title, channel_sync_state.channel_title),
         channel_thumbnail_url = COALESCE(excluded.channel_thumbnail_url, channel_sync_state.channel_thumbnail_url),
         description           = COALESCE(excluded.description, channel_sync_state.description)"
).bind(&body.channel_id)
 .bind(&title)
 .bind(&thumb)
 .bind(&description)
 .execute(&mut *tx).await?;
```

#### Why this fixes the burst risk

The dominant allowlist flow is: parent searches via `/api/parent/search`, clicks a result, UI POSTs to add. The search response already contained the channel title and thumbnail. With the body-data path, *zero* sidecar calls happen during the POST.

| Source of allowlist POST | Sidecar metadata calls before / after this change |
|---|---|
| Click a result from `/api/parent/search` (dominant path) | 1 → **0** |
| Paste a raw channel ID or URL (rare, manual) | 1 → 1 (sidecar fallback) |
| Bulk import / API automation without prior search | N → N (sidecar fallback) — addressable via per-IP rate limit if needed, but out of scope for this plan |

The remaining "paste a raw ID" path is single-channel-at-a-time, manual, and not a burst surface. Bulk automation that doesn't pre-search remains a theoretical risk but is not introduced by this plan and is best addressed as a separate cross-cutting rate-limit concern (out of scope).

#### Frontend changes (included in this plan)

The parent allowlist UI is a single Lit component, `frontend/src/components/allowlist-manager.ts` (493 lines, mounted as `<hometube-allowlist-manager child-id="...">`). Its `addItemForKind(item, kind)` method at lines **253-277** builds the POST body. The video branch (lines 265-270) **already** does exactly what we need to do for channels — it forwards `title`, `channel_title`, and `thumbnail_url` from the search result, with a documented server-side fallback contract (comment at 256-261).

Today's channel branch (line 264) is just:
```ts
{ channel_id: item.id }
```

Expand it to mirror the video branch:
```ts
{
  channel_id: item.id,
  channel_title: item.title,              // for channel-kind, the channel name is in .title, NOT .channel_title
  channel_thumbnail_url: pickThumbnail(item.thumbnails),
  description: item.description,
}
```

**Critical detail (`item.title` vs `item.channel_title`):** for **channel-kind** `SearchItem` results, the channel's display name is in `item.title`. The `item.channel_title` field is populated only for **video-kind** results (to name the *owning* channel of the video). Confused use of the wrong field will write empty/null titles for channels. The same field-mapping difference exists in the existing video branch (which correctly uses `item.title` for the video's own title and `item.channel_title` for the owning channel).

`pickThumbnail` (already imported at `allowlist-manager.ts:14-24`) and `SearchItem.description` (already defined at `frontend/src/types/index.ts:79-88`) are pre-existing; no new imports or type changes are required for this edit.

The sibling test file `frontend/src/components/allowlist-manager.test.ts` has an existing channel-POST test at lines 113-127 that asserts URL only (not body shape). Extend it (and/or the search-results test at 175-213) to assert the new body fields.

Backend-frontend compatibility: the new backend handler treats body metadata as optional with sidecar fallback, so this is a backward-compatible ship — backend can roll out independently and an old frontend will still work (just continues to incur the sidecar call). Once the frontend ship lands, the burst-protection benefit is active.

#### DELETE path

Unchanged in shape: the DELETE handler (`src/routes/allowlist.rs:116-146`) keeps the "GC if no other child has it" guard, but now drops from `channel_sync_state` (and cascades the `channel_videos` archive for that channel) instead of `feed_sources` / `feed_source_items`.

### Subscription-route wiring (`src/routes/subscriptions.rs`)

- `POST /api/subscriptions` (line 94) — after the existing upsert, run the priority bump:
  ```sql
  UPDATE channel_sync_state
  SET backfill_next_at = 0
  WHERE channel_id = ?1
    AND backfill_status = 'pending'
    AND backfill_last_completed_at IS NULL;
  ```
  Effect:
  - If the channel has never been backfilled (`backfill_last_completed_at IS NULL`), and is currently in the queue (`pending`), bump it to the front.
  - If the channel has already been backfilled, leave it on its natural 30-day re-backfill schedule. A subscription event doesn't justify an extra full subprocess.
  - If the channel is `'running'`, `'complete'`, `'failed'`, or `'shelved'`, no-op.
  - If the channel isn't in `channel_sync_state` at all (not allowlisted), no-op — the predicate just matches zero rows.

- `DELETE /api/subscriptions/:channelId` (line 131) — **no changes needed**. Subscriptions are decoupled from sync eligibility, so unsubscribing has no effect on backfill state.

### Resolving the "subscribe to a long-allowlisted channel" UX

The priority hint above handles this case: parent allowlists "Mark Rober" months ago, it eventually gets backfilled, child later subscribes — nothing happens (already done). Parent allowlists "Mark Rober" today, it's still in the queue waiting its turn, child subscribes tomorrow — backfill bumps to front, runs within the next loop tick.

For the edge case of a child subscribing during the brief window between allowlist POST and the first backfill iteration (where `backfill_status` could already be `'running'`), the priority hint is a no-op and the backfill simply completes on its current pass.
