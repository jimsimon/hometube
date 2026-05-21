-- Purge legacy sidecar-fallback feed rows that were ingested before
-- relative-time strings (e.g. "3 days ago") were parsed into a
-- numeric `published_at`. Those rows have `published_at IS NULL` and
-- the prior dashboard ORDER BY (`COALESCE(published_at, 0) DESC`)
-- sorted them behind every real RSS-timestamped item, making newly
-- uploaded videos disappear from the "New Videos" feed.
--
-- The accompanying code change (sidecar_item_to_row now parses the
-- relative string at ingest) means re-poll repopulates these rows
-- with a usable timestamp. We bump the affected sources'
-- `next_poll_at` to 0 so the refresher picks them up on its next
-- tick instead of waiting out the remainder of the current interval.
-- For sources whose `next_poll_at` was already in the past this is a
-- harmless no-op for scheduling (the refresher already considers
-- them due). sqlx wraps each .sql migration in a single transaction,
-- so the UPDATE + DELETE here apply atomically.
--
-- The DELETE is intentionally narrow: it only targets rows whose
-- `published_raw` looks like a relative-time string (contains "ago",
-- or is one of the known shorthand forms). RSS-ingest rows that
-- happen to have NULL `published_at` because the ISO-8601 parse
-- failed are left alone — those carry an ISO-8601-shaped string and
-- a refresh wouldn't re-parse them any differently, so deleting them
-- would just churn the cache.

UPDATE feed_sources
   SET next_poll_at = 0
 WHERE (kind, source_id) IN (
       SELECT DISTINCT kind, source_id
         FROM feed_source_items
        WHERE published_at IS NULL
          AND published_raw IS NOT NULL
          AND (
               published_raw LIKE '%ago%'
            OR LOWER(TRIM(published_raw)) IN ('just now', 'yesterday', 'a moment', 'moments')
          )
   );

DELETE FROM feed_source_items
 WHERE published_at IS NULL
   AND published_raw IS NOT NULL
   AND (
        published_raw LIKE '%ago%'
     OR LOWER(TRIM(published_raw)) IN ('just now', 'yesterday', 'a moment', 'moments')
   );
