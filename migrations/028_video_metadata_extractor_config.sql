-- yt-dlp direct media URLs depend on the extractor configuration that
-- produced them (player clients, cookies, PO-token provider, JS runtime,
-- and yt-dlp version). Preserve the configuration fingerprint alongside
-- each row so a deployment/configuration change cannot reuse incompatible
-- URLs from the previous process.
ALTER TABLE video_metadata_cache
ADD COLUMN extractor_config_key TEXT NOT NULL DEFAULT '';
