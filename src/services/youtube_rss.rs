//! YouTube channel-RSS poller.
//!
//! YouTube exposes an unauthenticated Atom feed of recent uploads at
//! `https://www.youtube.com/feeds/videos.xml?channel_id=...`. It returns
//! up to 15 entries, supports `ETag` / `If-Modified-Since` conditional
//! requests, and does not go through the InnerTube anti-bot path.
//!
//! This module:
//!
//! 1. Issues a conditional GET against the feed URL.
//! 2. On 304, returns [`PollOutcome::NotModified`] so the caller can
//!    update bookkeeping without writing rows.
//! 3. On 200, parses the Atom feed into a vector of
//!    [`crate::services::feed_cache::ItemRow`].
//!
//! The base URL is configurable via the `youtube_rss_base_url`
//! `app_config` key so integration tests can redirect traffic to
//! wiremock.

use reqwest::header::{ETAG, IF_MODIFIED_SINCE, IF_NONE_MATCH, LAST_MODIFIED};
use reqwest::{Client, StatusCode};
use sqlx::SqlitePool;
use tracing::debug;

use crate::error::{AppError, AppResult};
use crate::services::feed_cache::ItemRow;
use crate::services::setup::get_config_value;

/// `app_config` key that, when set, overrides the YouTube RSS base
/// URL. Integration tests point this at a wiremock host.
pub const KEY_RSS_BASE_URL: &str = "youtube_rss_base_url";

/// Default base URL used in production.
pub const DEFAULT_RSS_BASE_URL: &str = "https://www.youtube.com";

/// What the caller learned from one poll attempt.
#[derive(Debug)]
pub enum PollOutcome {
    /// HTTP 304 (Not Modified). No items returned; existing cached
    /// rows remain authoritative.
    NotModified,
    /// HTTP 200 with a freshly parsed feed.
    Updated {
        title: Option<String>,
        etag: Option<String>,
        last_modified: Option<String>,
        items: Vec<ItemRow>,
    },
}

/// Resolve the base URL from `app_config`, falling back to the public
/// YouTube host.
pub async fn base_url(pool: &SqlitePool) -> String {
    match get_config_value(pool, KEY_RSS_BASE_URL).await {
        Ok(Some(s)) if !s.is_empty() => s,
        _ => DEFAULT_RSS_BASE_URL.to_string(),
    }
}

/// Issue one conditional GET against the channel feed.
#[tracing::instrument(name = "feed.poll.rss", skip_all, fields(channel_id = %channel_id))]
pub async fn poll_channel(
    http: &Client,
    base: &str,
    channel_id: &str,
    etag: Option<&str>,
    last_modified: Option<&str>,
) -> AppResult<PollOutcome> {
    // Build the request through reqwest's typed query API rather than
    // string-interpolating `channel_id` into the URL — even though
    // YouTube channel IDs are constrained to a safe alphabet, the
    // helper takes care of percent-encoding for any future caller
    // that passes user-controlled input.
    let url = format!("{base}/feeds/videos.xml");
    let mut req = http.get(&url).query(&[("channel_id", channel_id)]);
    if let Some(tag) = etag {
        req = req.header(IF_NONE_MATCH, tag);
    }
    if let Some(modified) = last_modified {
        req = req.header(IF_MODIFIED_SINCE, modified);
    }

    let resp = req.send().await?;
    let status = resp.status();
    debug!(%channel_id, %status, "channel RSS poll");

    if status == StatusCode::NOT_MODIFIED {
        return Ok(PollOutcome::NotModified);
    }
    if !status.is_success() {
        return Err(AppError::Other(anyhow::anyhow!(
            "channel RSS GET returned status {status}"
        )));
    }

    let etag = resp
        .headers()
        .get(ETAG)
        .and_then(|h| h.to_str().ok())
        .map(|s| s.to_string());
    let last_modified = resp
        .headers()
        .get(LAST_MODIFIED)
        .and_then(|h| h.to_str().ok())
        .map(|s| s.to_string());
    let body = resp.text().await?;

    let (title, items) = parse_atom(&body)
        .map_err(|e| AppError::Other(anyhow::anyhow!("parsing channel feed: {e}")))?;

    Ok(PollOutcome::Updated {
        title,
        etag,
        last_modified,
        items,
    })
}

/// Parse a YouTube channel Atom feed into `(channel_title, items)`.
///
/// We use `quick-xml` in event-streaming mode. The feed layout is
/// fixed:
///
/// ```text
/// <feed>
///   <title>Channel Title</title>
///   <entry>
///     <yt:videoId>...</yt:videoId>
///     <yt:channelId>...</yt:channelId>
///     <title>...</title>
///     <author><name>...</name></author>
///     <published>...</published>
///     <media:group>
///       <media:thumbnail url="..."/>
///     </media:group>
///   </entry>
///   ...
/// </feed>
/// ```
pub fn parse_atom(xml: &str) -> Result<(Option<String>, Vec<ItemRow>), quick_xml::Error> {
    use quick_xml::events::Event;
    use quick_xml::Reader;

    let mut reader = Reader::from_str(xml);
    reader.config_mut().trim_text(true);

    let mut buf = Vec::new();
    let mut feed_title: Option<String> = None;
    let mut items: Vec<ItemRow> = Vec::new();

    // Per-entry accumulators.
    let mut in_entry = false;
    let mut in_author = false;
    let mut depth = 0u32;
    let mut current: Option<EntryAccum> = None;

    // Tracks which leaf text element we're currently inside (within an
    // entry). Used to route text events to the right field. Matching
    // is done on the *local* name (the part after the colon) so the
    // parser tolerates the publisher choosing any namespace prefix.
    let mut current_tag: Option<TextTarget> = None;

    loop {
        match reader.read_event_into(&mut buf)? {
            Event::Eof => break,
            Event::Start(e) => {
                depth += 1;
                let local = e.local_name();
                match local.as_ref() {
                    b"entry" => {
                        in_entry = true;
                        current = Some(EntryAccum::default());
                    }
                    b"author" if in_entry => {
                        in_author = true;
                    }
                    b"title" => {
                        current_tag = Some(if in_entry {
                            TextTarget::EntryTitle
                        } else {
                            TextTarget::FeedTitle
                        });
                    }
                    b"videoId" if in_entry => {
                        current_tag = Some(TextTarget::VideoId);
                    }
                    b"channelId" if in_entry => {
                        current_tag = Some(TextTarget::ChannelId);
                    }
                    b"name" if in_entry && in_author => {
                        current_tag = Some(TextTarget::AuthorName);
                    }
                    b"published" if in_entry => {
                        current_tag = Some(TextTarget::Published);
                    }
                    _ => {}
                }
            }
            Event::End(e) => {
                depth = depth.saturating_sub(1);
                let local = e.local_name();
                match local.as_ref() {
                    b"entry" => {
                        if let Some(acc) = current.take() {
                            if let Some(row) = acc.into_row() {
                                items.push(row);
                            }
                        }
                        in_entry = false;
                    }
                    b"author" => {
                        in_author = false;
                    }
                    b"title" | b"videoId" | b"channelId" | b"name" | b"published" => {
                        current_tag = None;
                    }
                    _ => {}
                }
            }
            Event::Empty(e) => {
                // <thumbnail url="..."/> is the self-closing element
                // (any namespace prefix — typically `media:`).
                let local = e.local_name();
                if in_entry && local.as_ref() == b"thumbnail" {
                    if let Some(acc) = current.as_mut() {
                        for attr in e.attributes().flatten() {
                            if attr.key.local_name().as_ref() == b"url" {
                                if let Ok(v) = attr.unescape_value() {
                                    // Prefer the first thumbnail we see.
                                    if acc.thumbnail.is_none() {
                                        acc.thumbnail = Some(v.into_owned());
                                    }
                                }
                            }
                        }
                    }
                }
            }
            Event::Text(t) => {
                let text = t.unescape().unwrap_or_default().into_owned();
                match current_tag {
                    // Only record the first non-empty title we see;
                    // YouTube's feed has a single <title> at feed level,
                    // but defending against repeats is cheap.
                    #[allow(clippy::collapsible_match)]
                    Some(TextTarget::FeedTitle) => {
                        if feed_title.is_none() && !text.is_empty() {
                            feed_title = Some(text);
                        }
                    }
                    Some(TextTarget::EntryTitle) => {
                        if let Some(acc) = current.as_mut() {
                            acc.title = Some(text);
                        }
                    }
                    Some(TextTarget::VideoId) => {
                        if let Some(acc) = current.as_mut() {
                            acc.video_id = Some(text);
                        }
                    }
                    Some(TextTarget::ChannelId) => {
                        if let Some(acc) = current.as_mut() {
                            acc.channel_id = Some(text);
                        }
                    }
                    Some(TextTarget::AuthorName) => {
                        if let Some(acc) = current.as_mut() {
                            acc.channel_title = Some(text);
                        }
                    }
                    Some(TextTarget::Published) => {
                        if let Some(acc) = current.as_mut() {
                            acc.published_raw = Some(text);
                        }
                    }
                    None => {}
                }
            }
            _ => {}
        }
        buf.clear();
    }

    Ok((feed_title, items))
}

#[derive(Default)]
struct EntryAccum {
    video_id: Option<String>,
    channel_id: Option<String>,
    channel_title: Option<String>,
    title: Option<String>,
    published_raw: Option<String>,
    thumbnail: Option<String>,
}

impl EntryAccum {
    fn into_row(self) -> Option<ItemRow> {
        let video_id = self.video_id?;
        let title = self.title.unwrap_or_default();
        let published_at = self
            .published_raw
            .as_deref()
            .and_then(parse_iso8601_to_unix);
        Some(ItemRow {
            video_id,
            title,
            channel_id: self.channel_id,
            channel_title: self.channel_title,
            thumbnail_url: self.thumbnail,
            published_at,
            published_raw: self.published_raw,
        })
    }
}

#[derive(Copy, Clone, Debug)]
enum TextTarget {
    FeedTitle,
    EntryTitle,
    VideoId,
    ChannelId,
    AuthorName,
    Published,
}

fn parse_iso8601_to_unix(s: &str) -> Option<i64> {
    // YouTube's `<published>` field is RFC 3339 / ISO 8601 with a
    // timezone offset. `chrono::DateTime::parse_from_rfc3339` handles it.
    chrono::DateTime::parse_from_rfc3339(s)
        .ok()
        .map(|dt| dt.timestamp())
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<feed xmlns:yt="http://www.youtube.com/xml/schemas/2015"
      xmlns:media="http://search.yahoo.com/mrss/"
      xmlns="http://www.w3.org/2005/Atom">
  <title>Cool Channel</title>
  <entry>
    <id>yt:video:abc123XYZ_-</id>
    <yt:videoId>abc123XYZ_-</yt:videoId>
    <yt:channelId>UC_test</yt:channelId>
    <title>Episode 1</title>
    <author>
      <name>Cool Channel</name>
    </author>
    <published>2024-05-01T12:00:00+00:00</published>
    <media:group>
      <media:thumbnail url="https://i.ytimg.com/vi/abc123XYZ_-/hqdefault.jpg" width="480" height="360"/>
    </media:group>
  </entry>
  <entry>
    <yt:videoId>def456</yt:videoId>
    <yt:channelId>UC_test</yt:channelId>
    <title>Episode 2</title>
    <author><name>Cool Channel</name></author>
    <published>2024-05-02T12:00:00+00:00</published>
    <media:group>
      <media:thumbnail url="https://i.ytimg.com/vi/def456/hqdefault.jpg"/>
    </media:group>
  </entry>
</feed>"#;

    #[test]
    fn parses_full_feed() {
        let (title, items) = parse_atom(SAMPLE).unwrap();
        assert_eq!(title.as_deref(), Some("Cool Channel"));
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].video_id, "abc123XYZ_-");
        assert_eq!(items[0].title, "Episode 1");
        assert_eq!(items[0].channel_id.as_deref(), Some("UC_test"));
        assert_eq!(items[0].channel_title.as_deref(), Some("Cool Channel"));
        assert_eq!(
            items[0].thumbnail_url.as_deref(),
            Some("https://i.ytimg.com/vi/abc123XYZ_-/hqdefault.jpg")
        );
        assert_eq!(
            items[0].published_raw.as_deref(),
            Some("2024-05-01T12:00:00+00:00")
        );
        assert_eq!(items[0].published_at, Some(1714564800));
    }

    #[test]
    fn parses_minimal_entry() {
        let xml = r#"<?xml version="1.0"?>
<feed xmlns="http://www.w3.org/2005/Atom" xmlns:yt="http://www.youtube.com/xml/schemas/2015">
  <title>X</title>
  <entry><yt:videoId>v1</yt:videoId><title>T</title></entry>
</feed>"#;
        let (title, items) = parse_atom(xml).unwrap();
        assert_eq!(title.as_deref(), Some("X"));
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].video_id, "v1");
        assert_eq!(items[0].published_at, None);
        assert!(items[0].thumbnail_url.is_none());
    }

    #[test]
    fn alternate_namespace_prefix_still_parses() {
        // Same feed but with `youtube:` instead of `yt:` and `m:`
        // instead of `media:` — both legal XML namespace bindings.
        let xml = r#"<?xml version="1.0"?>
<feed xmlns:youtube="http://www.youtube.com/xml/schemas/2015"
      xmlns:m="http://search.yahoo.com/mrss/"
      xmlns="http://www.w3.org/2005/Atom">
  <title>X</title>
  <entry>
    <youtube:videoId>v1</youtube:videoId>
    <title>T</title>
    <m:group><m:thumbnail url="https://t/v1.jpg"/></m:group>
  </entry>
</feed>"#;
        let (_, items) = parse_atom(xml).unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].video_id, "v1");
        assert_eq!(items[0].thumbnail_url.as_deref(), Some("https://t/v1.jpg"));
    }

    #[test]
    fn entry_without_videoid_is_dropped() {
        let xml = r#"<?xml version="1.0"?>
<feed xmlns="http://www.w3.org/2005/Atom" xmlns:yt="http://www.youtube.com/xml/schemas/2015">
  <entry><title>orphan</title></entry>
</feed>"#;
        let (_, items) = parse_atom(xml).unwrap();
        assert!(items.is_empty());
    }
}
