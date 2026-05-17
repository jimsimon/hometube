-- Add total_bytes column to format_box_ranges so we can serve Range responses
-- from the segment cache without querying upstream for the total file size.
ALTER TABLE format_box_ranges ADD COLUMN total_bytes INTEGER;
