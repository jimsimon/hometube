-- Add `channel_id` to watch_history so the continue-watching feed can
-- run a proper access check for videos surfaced via an allowlisted
-- channel (not just individually allowlisted videos).
--
-- Without this column, `src/routes/feed.rs::continue_watching` had no
-- way to tell `can_child_view` which channel a historical row came
-- from, so it fell back to `channel_id=None` and only individually-
-- allowlisted videos survived the filter. Channel-sourced videos were
-- silently dropped from the row.
--
-- We backfill from `feed_source_items` where we already have a
-- video_id → channel_id mapping for cached channel feeds. Rows that
-- can't be resolved are left NULL; `continue_watching` will fall back
-- to a per-row lookup so they still surface if/when the source
-- becomes resolvable.
-- Migration 009 already created `feed_source_items_video` on
-- `(video_id)`, so both the backfill below and the runtime fallback in
-- `continue_watching` are already O(log n). No new index needed.
ALTER TABLE watch_history ADD COLUMN channel_id TEXT;

UPDATE watch_history
SET channel_id = (
    SELECT channel_id FROM feed_source_items
    WHERE feed_source_items.video_id = watch_history.video_id
      AND feed_source_items.channel_id IS NOT NULL
    LIMIT 1
)
WHERE channel_id IS NULL;
