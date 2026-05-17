//! YouTube content discovery client.
//!
//! Wraps the discovery sidecar (powered by youtubei.js) used by parent-side
//! discovery (search, channel/playlist/video lookup) and by the inbound
//! feed generator. The sidecar communicates with YouTube's InnerTube API
//! directly, eliminating the dependency on the official YouTube Data API v3.
//!
//! ## Caching
//!
//! An in-memory response cache keyed by the canonical request URL with a
//! 10-minute TTL sits in front of the sidecar. The metadata cache
//! (`video_metadata_cache` table) is for yt-dlp extraction and is
//! unrelated to this layer.
//!
//! ## Safety
//!
//! HomeTube **never** fetches YouTube comments. There is no public method
//! in this module that requests comments from the sidecar. This is an
//! architectural safety guarantee, not a toggle.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use reqwest::Client;
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;
use tracing::debug;

use crate::error::{AppError, AppResult};
use crate::services::setup::get_config_value;
use sqlx::SqlitePool;

/// `app_config` key that, when set, overrides the discovery sidecar URL.
/// Used by integration tests to redirect traffic to a wiremock server.
const KEY_DISCOVERY_BASE_URL: &str = "discovery_sidecar_url";

/// Default sidecar URL (matches the Docker Compose service name).
const DEFAULT_SIDECAR_URL: &str = "http://discovery:3000";

/// In-memory response cache TTL (matches the typical "fresh enough for
/// browsing" window).
const CACHE_TTL: Duration = Duration::from_secs(10 * 60);

/// Allowed `type` filter passed to [`YoutubeClient::search`].
#[derive(Debug, Clone, Copy)]
pub enum SearchType {
    Channel,
    Playlist,
    Video,
}

impl SearchType {
    fn as_str(self) -> &'static str {
        match self {
            SearchType::Channel => "channel",
            SearchType::Playlist => "playlist",
            SearchType::Video => "video",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "channel" => Some(SearchType::Channel),
            "playlist" => Some(SearchType::Playlist),
            "video" => Some(SearchType::Video),
            _ => None,
        }
    }
}

/// One entry from `thumbnails`. Keys are sizes ("default", "medium",
/// "high", "standard", "maxres").
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ThumbnailInfo {
    pub url: String,
    #[serde(default)]
    pub width: Option<i64>,
    #[serde(default)]
    pub height: Option<i64>,
}

/// A normalised search result item.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchItem {
    /// `"channel"`, `"playlist"`, or `"video"`.
    pub kind: String,
    pub id: String,
    pub title: String,
    #[serde(default)]
    pub description: String,
    pub channel_id: Option<String>,
    pub channel_title: Option<String>,
    #[serde(default)]
    pub thumbnails: HashMap<String, ThumbnailInfo>,
    pub published_at: Option<String>,
}

/// A channel resource (subset).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChannelInfo {
    pub id: String,
    pub title: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub thumbnails: HashMap<String, ThumbnailInfo>,
    pub subscriber_count: Option<i64>,
    pub video_count: Option<i64>,
    /// Uploads playlist ID (`UU...`) — used for "latest videos" feed.
    pub uploads_playlist_id: Option<String>,
}

/// A playlist resource (subset).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlaylistInfo {
    pub id: String,
    pub title: String,
    #[serde(default)]
    pub description: String,
    pub channel_id: Option<String>,
    pub channel_title: Option<String>,
    #[serde(default)]
    pub thumbnails: HashMap<String, ThumbnailInfo>,
    pub item_count: Option<i64>,
}

/// A video resource (subset). Comments are intentionally absent.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VideoInfo {
    pub id: String,
    pub title: String,
    #[serde(default)]
    pub description: String,
    pub channel_id: Option<String>,
    pub channel_title: Option<String>,
    #[serde(default)]
    pub thumbnails: HashMap<String, ThumbnailInfo>,
    pub published_at: Option<String>,
    /// ISO 8601 duration (e.g. `PT4M13S`).
    pub duration: Option<String>,
    pub view_count: Option<i64>,
}

/// One row from a playlist items response — represents a video inside a
/// playlist.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlaylistItem {
    pub video_id: String,
    pub title: String,
    pub channel_id: Option<String>,
    pub channel_title: Option<String>,
    #[serde(default)]
    pub thumbnails: HashMap<String, ThumbnailInfo>,
    pub published_at: Option<String>,
    pub position: Option<i64>,
}

/// A page of items from a paginated endpoint.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Page<T> {
    pub items: Vec<T>,
    pub next_page_token: Option<String>,
}

/// Cached entry: timestamp of insertion + the JSON body.
#[derive(Clone)]
struct CacheEntry {
    inserted_at: Instant,
    body: serde_json::Value,
}

/// Content discovery client backed by the youtubei.js sidecar.
///
/// Replaces the former YouTube Data API v3 client. The public interface
/// is identical; only the transport layer changed.
#[derive(Clone)]
pub struct YoutubeClient {
    http: Client,
    cache: Arc<Mutex<HashMap<String, CacheEntry>>>,
    /// Base URL for the discovery sidecar. Defaults to
    /// [`DEFAULT_SIDECAR_URL`] but can be overridden for testing.
    base_url: String,
}

impl YoutubeClient {
    /// Construct a client from the sidecar URL configured in
    /// `app_config` or the `DISCOVERY_SIDECAR_URL` env var.
    ///
    /// If `discovery_sidecar_url` is set in `app_config`, use that
    /// (this allows integration tests to redirect traffic to a mock
    /// server). Otherwise, fall back to the env var, then the default.
    pub async fn from_db(pool: &SqlitePool) -> AppResult<Self> {
        let base_url = get_config_value(pool, KEY_DISCOVERY_BASE_URL)
            .await
            .ok()
            .flatten()
            .unwrap_or_else(|| {
                std::env::var("DISCOVERY_SIDECAR_URL")
                    .unwrap_or_else(|_| DEFAULT_SIDECAR_URL.to_string())
            });
        Ok(Self {
            http: Client::new(),
            cache: Arc::new(Mutex::new(HashMap::new())),
            base_url,
        })
    }

    /// Construct a client with a custom base URL (for testing with
    /// mock HTTP servers like wiremock).
    #[doc(hidden)]
    pub fn with_base_url(base_url: &str) -> Self {
        Self {
            http: Client::new(),
            cache: Arc::new(Mutex::new(HashMap::new())),
            base_url: base_url.to_string(),
        }
    }

    /// Issue a GET to the sidecar, returning the parsed JSON body.
    /// Results are cached for [`CACHE_TTL`].
    async fn get_json(&self, path: &str) -> AppResult<serde_json::Value> {
        let url = format!("{}{path}", self.base_url);

        // Try the cache first.
        {
            let mut cache = self.cache.lock().await;
            if let Some(entry) = cache.get(&url) {
                if entry.inserted_at.elapsed() < CACHE_TTL {
                    debug!(%url, "discovery cache hit");
                    return Ok(entry.body.clone());
                } else {
                    cache.remove(&url);
                }
            }
        }

        let res = self.http.get(&url).send().await.map_err(AppError::Http)?;
        let status = res.status();
        if !status.is_success() && status.as_u16() != 404 {
            // Non-success, non-404 → hard error.
            let body = res.text().await.unwrap_or_default();
            return Err(AppError::Other(anyhow::anyhow!(
                "Discovery sidecar {status}: {body}"
            )));
        }
        // 2xx and 404 are parsed as JSON — callers inspect the `error`
        // field to distinguish "resource not found" from success.
        let is_not_found = status.as_u16() == 404;
        let body: serde_json::Value = res.json().await.map_err(AppError::Http)?;

        // Only cache successful responses — a transient 404 shouldn't
        // stick around for the full TTL.
        if !is_not_found {
            let mut cache = self.cache.lock().await;
            cache.insert(
                url,
                CacheEntry {
                    inserted_at: Instant::now(),
                    body: body.clone(),
                },
            );
        }
        Ok(body)
    }

    /// Search for channels, playlists, or videos.
    pub async fn search(
        &self,
        q: &str,
        kind: SearchType,
        max_results: u32,
    ) -> AppResult<Vec<SearchItem>> {
        let encoded_q = percent_encode(q);
        let path = format!(
            "/search?q={}&type={}&maxResults={}",
            encoded_q,
            kind.as_str(),
            max_results.min(50)
        );
        let body = self.get_json(&path).await?;
        let items: Vec<SearchItem> = body
            .get("items")
            .and_then(|v| serde_json::from_value(v.clone()).ok())
            .unwrap_or_default();
        Ok(items)
    }

    /// Get metadata for a single channel.
    pub async fn get_channel(&self, id: &str) -> AppResult<Option<ChannelInfo>> {
        let path = format!("/channels/{}", percent_encode(id));
        let body = self.get_json(&path).await?;

        // The sidecar returns the channel object directly (not wrapped
        // in an items array). A 404 from the sidecar is surfaced as an
        // error by get_json; if we somehow get an error field, treat as
        // not found.
        if body.get("error").is_some() {
            return Ok(None);
        }
        Ok(serde_json::from_value(body).ok())
    }

    /// Get metadata for a single playlist.
    pub async fn get_playlist(&self, id: &str) -> AppResult<Option<PlaylistInfo>> {
        let path = format!("/playlists/{}", percent_encode(id));
        let body = self.get_json(&path).await?;
        if body.get("error").is_some() {
            return Ok(None);
        }
        Ok(serde_json::from_value(body).ok())
    }

    /// Get metadata for a single video.
    pub async fn get_video(&self, id: &str) -> AppResult<Option<VideoInfo>> {
        let path = format!("/videos/{}", percent_encode(id));
        let body = self.get_json(&path).await?;
        if body.get("error").is_some() {
            return Ok(None);
        }
        Ok(serde_json::from_value(body).ok())
    }

    /// List a channel's most recent uploads.
    ///
    /// Delegates to the sidecar's `/channel-videos/:channelId` endpoint
    /// which resolves the uploads playlist internally.
    pub async fn list_channel_videos(
        &self,
        channel_id: &str,
        max_results: u32,
        page_token: Option<&str>,
    ) -> AppResult<Page<PlaylistItem>> {
        let mut path = format!(
            "/channel-videos/{}?maxResults={}",
            percent_encode(channel_id),
            max_results.min(50)
        );
        if let Some(tok) = page_token {
            path.push_str(&format!("&pageToken={}", percent_encode(tok)));
        }
        let body = self.get_json(&path).await?;
        let page: Page<PlaylistItem> = serde_json::from_value(body).unwrap_or_else(|_| Page {
            items: Vec::new(),
            next_page_token: None,
        });
        Ok(page)
    }

    /// List the items of a playlist.
    pub async fn list_playlist_items(
        &self,
        playlist_id: &str,
        max_results: u32,
        page_token: Option<&str>,
    ) -> AppResult<Page<PlaylistItem>> {
        let mut path = format!(
            "/playlist-items/{}?maxResults={}",
            percent_encode(playlist_id),
            max_results.min(50)
        );
        if let Some(tok) = page_token {
            path.push_str(&format!("&pageToken={}", percent_encode(tok)));
        }
        let body = self.get_json(&path).await?;
        let page: Page<PlaylistItem> = serde_json::from_value(body).unwrap_or_else(|_| Page {
            items: Vec::new(),
            next_page_token: None,
        });
        Ok(page)
    }
}

// ---------------------------------------------------------------------------
// Utility helpers
// ---------------------------------------------------------------------------

/// Tiny RFC 3986 unreserved-set percent-encoder. We avoid pulling in
/// `urlencoding` for one helper.
pub(crate) fn percent_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char);
            }
            _ => {
                out.push_str(&format!("%{b:02X}"));
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    // -----------------------------------------------------------------------
    // percent_encode
    // -----------------------------------------------------------------------

    #[test]
    fn encode_unreserved_chars_unchanged() {
        assert_eq!(percent_encode("abcXYZ0189-_.~"), "abcXYZ0189-_.~");
    }

    #[test]
    fn encode_special_chars() {
        assert_eq!(percent_encode("hello world"), "hello%20world");
        assert_eq!(percent_encode("a&b=c"), "a%26b%3Dc");
        assert_eq!(percent_encode("/path"), "%2Fpath");
    }

    #[test]
    fn encode_empty_string() {
        assert_eq!(percent_encode(""), "");
    }

    // -----------------------------------------------------------------------
    // SearchType
    // -----------------------------------------------------------------------

    #[test]
    fn search_type_as_str_round_trips() {
        assert_eq!(SearchType::Channel.as_str(), "channel");
        assert_eq!(SearchType::Playlist.as_str(), "playlist");
        assert_eq!(SearchType::Video.as_str(), "video");
    }

    // -----------------------------------------------------------------------
    // Deserialization of sidecar responses
    // -----------------------------------------------------------------------

    #[test]
    fn deserialize_search_item() {
        let json = serde_json::json!({
            "kind": "video",
            "id": "abc123",
            "title": "Test Video",
            "description": "A test",
            "channel_id": "UCxyz",
            "channel_title": "Test Channel",
            "thumbnails": {
                "default": {"url": "https://img/d.jpg", "width": 120, "height": 90}
            },
            "published_at": "2024-01-01T00:00:00Z"
        });
        let item: SearchItem = serde_json::from_value(json).unwrap();
        assert_eq!(item.kind, "video");
        assert_eq!(item.id, "abc123");
        assert_eq!(item.title, "Test Video");
        assert_eq!(item.channel_id, Some("UCxyz".into()));
    }

    #[test]
    fn deserialize_channel_info() {
        let json = serde_json::json!({
            "id": "UCabc",
            "title": "Channel",
            "description": "About",
            "thumbnails": {"default": {"url": "http://t/d.jpg"}},
            "subscriber_count": 1000,
            "video_count": 50,
            "uploads_playlist_id": "UUabc"
        });
        let ch: ChannelInfo = serde_json::from_value(json).unwrap();
        assert_eq!(ch.id, "UCabc");
        assert_eq!(ch.title, "Channel");
        assert_eq!(ch.subscriber_count, Some(1000));
        assert_eq!(ch.uploads_playlist_id, Some("UUabc".into()));
    }

    #[test]
    fn deserialize_playlist_info() {
        let json = serde_json::json!({
            "id": "PLabc",
            "title": "Playlist",
            "description": "desc",
            "channel_id": "UCx",
            "channel_title": "ChX",
            "thumbnails": {},
            "item_count": 42
        });
        let pl: PlaylistInfo = serde_json::from_value(json).unwrap();
        assert_eq!(pl.id, "PLabc");
        assert_eq!(pl.title, "Playlist");
        assert_eq!(pl.item_count, Some(42));
    }

    #[test]
    fn deserialize_video_info() {
        let json = serde_json::json!({
            "id": "vidXYZ",
            "title": "Video Title",
            "description": "desc",
            "channel_id": "UCx",
            "channel_title": "Ch",
            "thumbnails": {},
            "published_at": "2024-06-01T12:00:00Z",
            "duration": "PT4M13S",
            "view_count": 12345
        });
        let v: VideoInfo = serde_json::from_value(json).unwrap();
        assert_eq!(v.id, "vidXYZ");
        assert_eq!(v.duration, Some("PT4M13S".into()));
        assert_eq!(v.view_count, Some(12345));
    }

    #[test]
    fn deserialize_playlist_item() {
        let json = serde_json::json!({
            "video_id": "vid123",
            "title": "Item",
            "channel_id": "UCowner",
            "channel_title": "Owner",
            "thumbnails": {},
            "published_at": "2024-01-01T00:00:00Z",
            "position": 3
        });
        let item: PlaylistItem = serde_json::from_value(json).unwrap();
        assert_eq!(item.video_id, "vid123");
        assert_eq!(item.title, "Item");
        assert_eq!(item.position, Some(3));
    }

    #[test]
    fn deserialize_page() {
        let json = serde_json::json!({
            "items": [
                {"video_id": "v1", "title": "V1", "thumbnails": {}},
                {"video_id": "v2", "title": "V2", "thumbnails": {}}
            ],
            "next_page_token": "tok123"
        });
        let page: Page<PlaylistItem> = serde_json::from_value(json).unwrap();
        assert_eq!(page.items.len(), 2);
        assert_eq!(page.next_page_token, Some("tok123".into()));
    }

    // -----------------------------------------------------------------------
    // ThumbnailInfo
    // -----------------------------------------------------------------------

    #[test]
    fn thumbnail_info_deserializes_from_json() {
        let json = r#"{"url":"https://i.ytimg.com/vi/abc/default.jpg","width":120,"height":90}"#;
        let info: ThumbnailInfo = serde_json::from_str(json).unwrap();
        assert_eq!(info.url, "https://i.ytimg.com/vi/abc/default.jpg");
        assert_eq!(info.width, Some(120));
        assert_eq!(info.height, Some(90));
    }

    #[test]
    fn thumbnail_info_deserializes_without_optional_fields() {
        let json = r#"{"url":"https://i.ytimg.com/vi/abc/default.jpg"}"#;
        let info: ThumbnailInfo = serde_json::from_str(json).unwrap();
        assert_eq!(info.url, "https://i.ytimg.com/vi/abc/default.jpg");
        assert_eq!(info.width, None);
        assert_eq!(info.height, None);
    }
}
