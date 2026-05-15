//! YouTube Data API v3 client.
//!
//! Thin wrapper around the public Data API used by parent-side discovery
//! (search, channel/playlist/video lookup) and by the inbound feed
//! generator. Authenticated calls (subscriptions, playlists,
//! likes-on-behalf-of-the-child) live in a separate sync service in a
//! later phase — this module only deals with read endpoints that take an
//! API key.
//!
//! ## Caching
//!
//! Phase 4 ships an in-memory response cache keyed by the canonical
//! request URL with a 10-minute TTL. The Phase 5 metadata cache (`video_metadata_cache`
//! table) is for yt-dlp extraction and is unrelated to this layer.
//!
//! ## Safety
//!
//! HomeTube **never** fetches YouTube comments. The Comments resource is
//! deliberately not exposed by this module and there is no public method
//! that calls `/youtube/v3/comments` or `/commentThreads`. This is an
//! architectural safety guarantee, not a toggle.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use reqwest::Client;
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;
use tracing::debug;

use crate::error::{AppError, AppResult};
use crate::services::setup::{get_config_value, KEY_YOUTUBE_API_KEY};
use sqlx::SqlitePool;

/// `app_config` key that, when set, overrides the YouTube API base URL.
/// Used by integration tests to redirect traffic to a wiremock server.
const KEY_YOUTUBE_API_BASE_URL: &str = "youtube_api_base_url";

/// Base URL for the YouTube Data API v3.
const API_BASE: &str = "https://www.googleapis.com/youtube/v3";

/// In-memory response cache TTL (matches the YouTube docs' typical "fresh
/// enough for browsing" window).
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
#[derive(Debug, Clone, Serialize)]
pub struct SearchItem {
    /// `"channel"`, `"playlist"`, or `"video"`.
    pub kind: String,
    pub id: String,
    pub title: String,
    pub description: String,
    pub channel_id: Option<String>,
    pub channel_title: Option<String>,
    pub thumbnails: HashMap<String, ThumbnailInfo>,
    pub published_at: Option<String>,
}

/// A channel resource (subset).
#[derive(Debug, Clone, Serialize)]
pub struct ChannelInfo {
    pub id: String,
    pub title: String,
    pub description: String,
    pub thumbnails: HashMap<String, ThumbnailInfo>,
    pub subscriber_count: Option<i64>,
    pub video_count: Option<i64>,
    /// Uploads playlist ID (`UU...`) — used for "latest videos" feed.
    pub uploads_playlist_id: Option<String>,
}

/// A playlist resource (subset).
#[derive(Debug, Clone, Serialize)]
pub struct PlaylistInfo {
    pub id: String,
    pub title: String,
    pub description: String,
    pub channel_id: Option<String>,
    pub channel_title: Option<String>,
    pub thumbnails: HashMap<String, ThumbnailInfo>,
    pub item_count: Option<i64>,
}

/// A video resource (subset). Comments are intentionally absent.
#[derive(Debug, Clone, Serialize)]
pub struct VideoInfo {
    pub id: String,
    pub title: String,
    pub description: String,
    pub channel_id: Option<String>,
    pub channel_title: Option<String>,
    pub thumbnails: HashMap<String, ThumbnailInfo>,
    pub published_at: Option<String>,
    /// ISO 8601 duration (e.g. `PT4M13S`).
    pub duration: Option<String>,
    pub view_count: Option<i64>,
}

/// One row from `playlistItems.list` — represents a video inside a
/// playlist.
#[derive(Debug, Clone, Serialize)]
pub struct PlaylistItem {
    pub video_id: String,
    pub title: String,
    pub channel_id: Option<String>,
    pub channel_title: Option<String>,
    pub thumbnails: HashMap<String, ThumbnailInfo>,
    pub published_at: Option<String>,
    pub position: Option<i64>,
}

/// A page of items from a paginated YouTube list endpoint.
#[derive(Debug, Clone, Serialize)]
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

/// Read-only YouTube Data API client.
#[derive(Clone)]
pub struct YoutubeClient {
    api_key: String,
    http: Client,
    cache: Arc<Mutex<HashMap<String, CacheEntry>>>,
    /// Base URL for the YouTube Data API. Defaults to [`API_BASE`] but
    /// can be overridden for testing (e.g. to point at a wiremock server).
    base_url: String,
}

impl YoutubeClient {
    /// Construct a client from the API key stored in `app_config`.
    ///
    /// If `youtube_api_base_url` is set in `app_config`, use that
    /// instead of the default Google endpoint. This allows integration
    /// tests to redirect traffic to a mock server.
    pub async fn from_db(pool: &SqlitePool) -> AppResult<Self> {
        let api_key = get_config_value(pool, KEY_YOUTUBE_API_KEY)
            .await?
            .ok_or_else(|| AppError::BadRequest("YouTube API key not configured".into()))?;
        let base_url = get_config_value(pool, KEY_YOUTUBE_API_BASE_URL)
            .await
            .ok()
            .flatten()
            .unwrap_or_else(|| API_BASE.to_string());
        Ok(Self {
            api_key,
            http: Client::new(),
            cache: Arc::new(Mutex::new(HashMap::new())),
            base_url,
        })
    }

    /// Construct a client with a custom base URL (for testing with
    /// mock HTTP servers like wiremock).
    #[doc(hidden)]
    pub fn with_base_url(api_key: &str, base_url: &str) -> Self {
        Self {
            api_key: api_key.to_string(),
            http: Client::new(),
            cache: Arc::new(Mutex::new(HashMap::new())),
            base_url: base_url.to_string(),
        }
    }

    /// Issue a GET to `path` with `query` (already including `key`),
    /// returning the parsed JSON body. Results are cached for
    /// [`CACHE_TTL`].
    async fn get_json(
        &self,
        path: &str,
        query: Vec<(&str, String)>,
    ) -> AppResult<serde_json::Value> {
        let mut full_query = query;
        full_query.push(("key", self.api_key.clone()));
        let url = build_canonical_url(&format!("{}{path}", self.base_url), &full_query);

        // Try the cache first.
        {
            let mut cache = self.cache.lock().await;
            if let Some(entry) = cache.get(&url) {
                if entry.inserted_at.elapsed() < CACHE_TTL {
                    debug!(%url, "youtube cache hit");
                    return Ok(entry.body.clone());
                } else {
                    cache.remove(&url);
                }
            }
        }

        let res = self.http.get(&url).send().await.map_err(AppError::Http)?;
        if !res.status().is_success() {
            let status = res.status();
            let body = res.text().await.unwrap_or_default();
            return Err(AppError::Other(anyhow::anyhow!(
                "YouTube API {status}: {body}"
            )));
        }
        let body: serde_json::Value = res.json().await.map_err(AppError::Http)?;

        {
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

    /// `search.list` — discovery endpoint used by the parent allowlist UI.
    pub async fn search(
        &self,
        q: &str,
        kind: SearchType,
        max_results: u32,
    ) -> AppResult<Vec<SearchItem>> {
        let body = self
            .get_json(
                "/search",
                vec![
                    ("part", "snippet".into()),
                    ("q", q.to_string()),
                    ("type", kind.as_str().into()),
                    ("maxResults", max_results.min(50).to_string()),
                    ("safeSearch", "strict".into()),
                ],
            )
            .await?;

        let items = body
            .get("items")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();
        Ok(items.iter().filter_map(parse_search_item).collect())
    }

    /// `channels.list` for a single channel ID.
    pub async fn get_channel(&self, id: &str) -> AppResult<Option<ChannelInfo>> {
        let body = self
            .get_json(
                "/channels",
                vec![
                    ("part", "snippet,statistics,contentDetails".into()),
                    ("id", id.to_string()),
                    ("maxResults", "1".into()),
                ],
            )
            .await?;
        let item = body
            .get("items")
            .and_then(|v| v.as_array())
            .and_then(|a| a.first())
            .cloned();
        Ok(item.as_ref().and_then(parse_channel))
    }

    /// `playlists.list` for a single playlist ID.
    pub async fn get_playlist(&self, id: &str) -> AppResult<Option<PlaylistInfo>> {
        let body = self
            .get_json(
                "/playlists",
                vec![
                    ("part", "snippet,contentDetails".into()),
                    ("id", id.to_string()),
                    ("maxResults", "1".into()),
                ],
            )
            .await?;
        let item = body
            .get("items")
            .and_then(|v| v.as_array())
            .and_then(|a| a.first())
            .cloned();
        Ok(item.as_ref().and_then(parse_playlist))
    }

    /// `videos.list` for a single video ID.
    pub async fn get_video(&self, id: &str) -> AppResult<Option<VideoInfo>> {
        let body = self
            .get_json(
                "/videos",
                vec![
                    ("part", "snippet,contentDetails,statistics".into()),
                    ("id", id.to_string()),
                    ("maxResults", "1".into()),
                ],
            )
            .await?;
        let item = body
            .get("items")
            .and_then(|v| v.as_array())
            .and_then(|a| a.first())
            .cloned();
        Ok(item.as_ref().and_then(parse_video))
    }

    /// List a channel's most recent uploads via its uploads playlist.
    /// Falls back to `search.list` if the channel resource doesn't expose
    /// the uploads playlist (rare, e.g. private channels).
    pub async fn list_channel_videos(
        &self,
        channel_id: &str,
        max_results: u32,
        page_token: Option<&str>,
    ) -> AppResult<Page<PlaylistItem>> {
        let channel = self.get_channel(channel_id).await?;
        if let Some(uploads) = channel.and_then(|c| c.uploads_playlist_id) {
            return self
                .list_playlist_items(&uploads, max_results, page_token)
                .await;
        }

        // Fallback: search by channel ID, ordered by date.
        let mut query = vec![
            ("part", "snippet".into()),
            ("channelId", channel_id.to_string()),
            ("type", "video".into()),
            ("order", "date".into()),
            ("maxResults", max_results.min(50).to_string()),
            ("safeSearch", "strict".into()),
        ];
        if let Some(tok) = page_token {
            query.push(("pageToken", tok.to_string()));
        }
        let body = self.get_json("/search", query).await?;
        let items: Vec<PlaylistItem> = body
            .get("items")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default()
            .iter()
            .filter_map(parse_search_item_as_playlist_item)
            .collect();
        let next_page_token = body
            .get("nextPageToken")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());
        Ok(Page {
            items,
            next_page_token,
        })
    }

    /// List the items of a playlist (`playlistItems.list`).
    pub async fn list_playlist_items(
        &self,
        playlist_id: &str,
        max_results: u32,
        page_token: Option<&str>,
    ) -> AppResult<Page<PlaylistItem>> {
        let mut query = vec![
            ("part", "snippet,contentDetails".into()),
            ("playlistId", playlist_id.to_string()),
            ("maxResults", max_results.min(50).to_string()),
        ];
        if let Some(tok) = page_token {
            query.push(("pageToken", tok.to_string()));
        }
        let body = self.get_json("/playlistItems", query).await?;
        let items: Vec<PlaylistItem> = body
            .get("items")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default()
            .iter()
            .filter_map(parse_playlist_item)
            .collect();
        let next_page_token = body
            .get("nextPageToken")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());
        Ok(Page {
            items,
            next_page_token,
        })
    }
}

// ---------------------------------------------------------------------------
// Parsing helpers
// ---------------------------------------------------------------------------

fn parse_thumbnails(v: &serde_json::Value) -> HashMap<String, ThumbnailInfo> {
    let mut out = HashMap::new();
    if let Some(obj) = v.as_object() {
        for (key, val) in obj {
            if let Ok(info) = serde_json::from_value::<ThumbnailInfo>(val.clone()) {
                out.insert(key.clone(), info);
            }
        }
    }
    out
}

fn parse_search_item(v: &serde_json::Value) -> Option<SearchItem> {
    let id_obj = v.get("id")?;
    let kind = id_obj.get("kind").and_then(|k| k.as_str()).unwrap_or("");
    let (kind_short, id) = match kind {
        "youtube#channel" => ("channel", id_obj.get("channelId")?.as_str()?),
        "youtube#playlist" => ("playlist", id_obj.get("playlistId")?.as_str()?),
        "youtube#video" => ("video", id_obj.get("videoId")?.as_str()?),
        _ => return None,
    };
    let snip = v.get("snippet")?;
    Some(SearchItem {
        kind: kind_short.to_string(),
        id: id.to_string(),
        title: snip
            .get("title")
            .and_then(|x| x.as_str())
            .unwrap_or("")
            .to_string(),
        description: snip
            .get("description")
            .and_then(|x| x.as_str())
            .unwrap_or("")
            .to_string(),
        channel_id: snip
            .get("channelId")
            .and_then(|x| x.as_str())
            .map(String::from),
        channel_title: snip
            .get("channelTitle")
            .and_then(|x| x.as_str())
            .map(String::from),
        thumbnails: snip
            .get("thumbnails")
            .map(parse_thumbnails)
            .unwrap_or_default(),
        published_at: snip
            .get("publishedAt")
            .and_then(|x| x.as_str())
            .map(String::from),
    })
}

fn parse_search_item_as_playlist_item(v: &serde_json::Value) -> Option<PlaylistItem> {
    let id = v.get("id")?.get("videoId")?.as_str()?.to_string();
    let snip = v.get("snippet")?;
    Some(PlaylistItem {
        video_id: id,
        title: snip
            .get("title")
            .and_then(|x| x.as_str())
            .unwrap_or("")
            .to_string(),
        channel_id: snip
            .get("channelId")
            .and_then(|x| x.as_str())
            .map(String::from),
        channel_title: snip
            .get("channelTitle")
            .and_then(|x| x.as_str())
            .map(String::from),
        thumbnails: snip
            .get("thumbnails")
            .map(parse_thumbnails)
            .unwrap_or_default(),
        published_at: snip
            .get("publishedAt")
            .and_then(|x| x.as_str())
            .map(String::from),
        position: None,
    })
}

fn parse_channel(v: &serde_json::Value) -> Option<ChannelInfo> {
    let id = v.get("id")?.as_str()?.to_string();
    let snip = v.get("snippet")?;
    let stats = v.get("statistics");
    let content = v.get("contentDetails");
    Some(ChannelInfo {
        id,
        title: snip
            .get("title")
            .and_then(|x| x.as_str())
            .unwrap_or("")
            .to_string(),
        description: snip
            .get("description")
            .and_then(|x| x.as_str())
            .unwrap_or("")
            .to_string(),
        thumbnails: snip
            .get("thumbnails")
            .map(parse_thumbnails)
            .unwrap_or_default(),
        subscriber_count: stats
            .and_then(|s| s.get("subscriberCount"))
            .and_then(|s| s.as_str())
            .and_then(|s| s.parse().ok()),
        video_count: stats
            .and_then(|s| s.get("videoCount"))
            .and_then(|s| s.as_str())
            .and_then(|s| s.parse().ok()),
        uploads_playlist_id: content
            .and_then(|c| c.get("relatedPlaylists"))
            .and_then(|p| p.get("uploads"))
            .and_then(|u| u.as_str())
            .map(String::from),
    })
}

fn parse_playlist(v: &serde_json::Value) -> Option<PlaylistInfo> {
    let id = v.get("id")?.as_str()?.to_string();
    let snip = v.get("snippet")?;
    let content = v.get("contentDetails");
    Some(PlaylistInfo {
        id,
        title: snip
            .get("title")
            .and_then(|x| x.as_str())
            .unwrap_or("")
            .to_string(),
        description: snip
            .get("description")
            .and_then(|x| x.as_str())
            .unwrap_or("")
            .to_string(),
        channel_id: snip
            .get("channelId")
            .and_then(|x| x.as_str())
            .map(String::from),
        channel_title: snip
            .get("channelTitle")
            .and_then(|x| x.as_str())
            .map(String::from),
        thumbnails: snip
            .get("thumbnails")
            .map(parse_thumbnails)
            .unwrap_or_default(),
        item_count: content
            .and_then(|c| c.get("itemCount"))
            .and_then(|n| n.as_i64()),
    })
}

fn parse_video(v: &serde_json::Value) -> Option<VideoInfo> {
    let id = v.get("id")?.as_str()?.to_string();
    let snip = v.get("snippet")?;
    let stats = v.get("statistics");
    let content = v.get("contentDetails");
    Some(VideoInfo {
        id,
        title: snip
            .get("title")
            .and_then(|x| x.as_str())
            .unwrap_or("")
            .to_string(),
        description: snip
            .get("description")
            .and_then(|x| x.as_str())
            .unwrap_or("")
            .to_string(),
        channel_id: snip
            .get("channelId")
            .and_then(|x| x.as_str())
            .map(String::from),
        channel_title: snip
            .get("channelTitle")
            .and_then(|x| x.as_str())
            .map(String::from),
        thumbnails: snip
            .get("thumbnails")
            .map(parse_thumbnails)
            .unwrap_or_default(),
        published_at: snip
            .get("publishedAt")
            .and_then(|x| x.as_str())
            .map(String::from),
        duration: content
            .and_then(|c| c.get("duration"))
            .and_then(|d| d.as_str())
            .map(String::from),
        view_count: stats
            .and_then(|s| s.get("viewCount"))
            .and_then(|s| s.as_str())
            .and_then(|s| s.parse().ok()),
    })
}

fn parse_playlist_item(v: &serde_json::Value) -> Option<PlaylistItem> {
    let snip = v.get("snippet")?;
    let video_id = v
        .get("contentDetails")
        .and_then(|c| c.get("videoId"))
        .and_then(|s| s.as_str())
        .or_else(|| {
            snip.get("resourceId")
                .and_then(|r| r.get("videoId"))
                .and_then(|s| s.as_str())
        })?;
    Some(PlaylistItem {
        video_id: video_id.to_string(),
        title: snip
            .get("title")
            .and_then(|x| x.as_str())
            .unwrap_or("")
            .to_string(),
        channel_id: snip
            .get("videoOwnerChannelId")
            .and_then(|x| x.as_str())
            .or_else(|| snip.get("channelId").and_then(|x| x.as_str()))
            .map(String::from),
        channel_title: snip
            .get("videoOwnerChannelTitle")
            .and_then(|x| x.as_str())
            .or_else(|| snip.get("channelTitle").and_then(|x| x.as_str()))
            .map(String::from),
        thumbnails: snip
            .get("thumbnails")
            .map(parse_thumbnails)
            .unwrap_or_default(),
        published_at: snip
            .get("publishedAt")
            .and_then(|x| x.as_str())
            .map(String::from),
        position: snip.get("position").and_then(|p| p.as_i64()),
    })
}

/// Build a deterministic URL for cache keying. Query params are sorted by
/// key, then by value, and percent-encoded. The `key` (API key) is
/// included so two clients with different keys do not share a cache slot.
pub(crate) fn build_canonical_url(base: &str, query: &[(&str, String)]) -> String {
    let mut pairs: Vec<(&str, &str)> = query.iter().map(|(k, v)| (*k, v.as_str())).collect();
    pairs.sort();
    let qs = pairs
        .iter()
        .map(|(k, v)| format!("{}={}", percent_encode(k), percent_encode(v)))
        .collect::<Vec<_>>()
        .join("&");
    format!("{base}?{qs}")
}

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
    // build_canonical_url
    // -----------------------------------------------------------------------

    #[test]
    fn canonical_url_sorted_params() {
        let url = build_canonical_url(
            "https://api.example.com/items",
            &[("z", "1".into()), ("a", "2".into())],
        );
        // Keys should be sorted alphabetically.
        assert!(url.contains("a=2&z=1"), "got: {url}");
    }

    #[test]
    fn canonical_url_encodes_values() {
        let url = build_canonical_url("https://api.example.com", &[("q", "hello world".into())]);
        assert!(url.contains("q=hello%20world"), "got: {url}");
    }

    // -----------------------------------------------------------------------
    // parse_thumbnails
    // -----------------------------------------------------------------------

    #[test]
    fn parse_thumbnails_from_json() {
        let json = serde_json::json!({
            "default": {"url": "https://img/d.jpg", "width": 120, "height": 90},
            "high": {"url": "https://img/h.jpg", "width": 480, "height": 360}
        });
        let map = parse_thumbnails(&json);
        assert_eq!(map.len(), 2);
        assert_eq!(map["default"].url, "https://img/d.jpg");
        assert_eq!(map["default"].width, Some(120));
        assert_eq!(map["high"].url, "https://img/h.jpg");
    }

    #[test]
    fn parse_thumbnails_empty_object() {
        let json = serde_json::json!({});
        let map = parse_thumbnails(&json);
        assert!(map.is_empty());
    }

    #[test]
    fn parse_thumbnails_non_object() {
        let json = serde_json::json!("not an object");
        let map = parse_thumbnails(&json);
        assert!(map.is_empty());
    }

    // -----------------------------------------------------------------------
    // parse_search_item
    // -----------------------------------------------------------------------

    #[test]
    fn parse_search_item_video() {
        let json = serde_json::json!({
            "id": {"kind": "youtube#video", "videoId": "abc123"},
            "snippet": {
                "title": "Test Video",
                "description": "A test",
                "channelId": "UCxyz",
                "channelTitle": "Test Channel",
                "thumbnails": {},
                "publishedAt": "2024-01-01T00:00:00Z"
            }
        });
        let item = parse_search_item(&json).unwrap();
        assert_eq!(item.kind, "video");
        assert_eq!(item.id, "abc123");
        assert_eq!(item.title, "Test Video");
        assert_eq!(item.channel_id, Some("UCxyz".into()));
    }

    #[test]
    fn parse_search_item_channel() {
        let json = serde_json::json!({
            "id": {"kind": "youtube#channel", "channelId": "UCabc"},
            "snippet": {
                "title": "My Channel",
                "description": "",
                "thumbnails": {}
            }
        });
        let item = parse_search_item(&json).unwrap();
        assert_eq!(item.kind, "channel");
        assert_eq!(item.id, "UCabc");
    }

    #[test]
    fn parse_search_item_playlist() {
        let json = serde_json::json!({
            "id": {"kind": "youtube#playlist", "playlistId": "PLxyz"},
            "snippet": {
                "title": "My Playlist",
                "description": "fun",
                "thumbnails": {}
            }
        });
        let item = parse_search_item(&json).unwrap();
        assert_eq!(item.kind, "playlist");
        assert_eq!(item.id, "PLxyz");
    }

    #[test]
    fn parse_search_item_missing_id_returns_none() {
        let json =
            serde_json::json!({"snippet": {"title": "x", "description": "", "thumbnails": {}}});
        assert!(parse_search_item(&json).is_none());
    }

    // -----------------------------------------------------------------------
    // parse_channel
    // -----------------------------------------------------------------------

    #[test]
    fn parse_channel_full() {
        let json = serde_json::json!({
            "id": "UCabc",
            "snippet": {
                "title": "Channel",
                "description": "About",
                "thumbnails": {"default": {"url": "http://t/d.jpg"}}
            },
            "statistics": {
                "subscriberCount": "1000",
                "videoCount": "50"
            },
            "contentDetails": {
                "relatedPlaylists": {"uploads": "UUabc"}
            }
        });
        let ch = parse_channel(&json).unwrap();
        assert_eq!(ch.id, "UCabc");
        assert_eq!(ch.title, "Channel");
        assert_eq!(ch.subscriber_count, Some(1000));
        assert_eq!(ch.video_count, Some(50));
        assert_eq!(ch.uploads_playlist_id, Some("UUabc".into()));
    }

    #[test]
    fn parse_channel_minimal() {
        let json = serde_json::json!({
            "id": "UCmin",
            "snippet": {"title": "Min", "description": "", "thumbnails": {}}
        });
        let ch = parse_channel(&json).unwrap();
        assert_eq!(ch.id, "UCmin");
        assert_eq!(ch.subscriber_count, None);
        assert_eq!(ch.uploads_playlist_id, None);
    }

    // -----------------------------------------------------------------------
    // parse_playlist
    // -----------------------------------------------------------------------

    #[test]
    fn parse_playlist_full() {
        let json = serde_json::json!({
            "id": "PLabc",
            "snippet": {
                "title": "Playlist",
                "description": "desc",
                "channelId": "UCx",
                "channelTitle": "ChX",
                "thumbnails": {}
            },
            "contentDetails": {"itemCount": 42}
        });
        let pl = parse_playlist(&json).unwrap();
        assert_eq!(pl.id, "PLabc");
        assert_eq!(pl.title, "Playlist");
        assert_eq!(pl.item_count, Some(42));
        assert_eq!(pl.channel_id, Some("UCx".into()));
    }

    // -----------------------------------------------------------------------
    // parse_video
    // -----------------------------------------------------------------------

    #[test]
    fn parse_video_full() {
        let json = serde_json::json!({
            "id": "vidXYZ",
            "snippet": {
                "title": "Video Title",
                "description": "desc",
                "channelId": "UCx",
                "channelTitle": "Ch",
                "thumbnails": {},
                "publishedAt": "2024-06-01T12:00:00Z"
            },
            "statistics": {"viewCount": "12345"},
            "contentDetails": {"duration": "PT4M13S"}
        });
        let v = parse_video(&json).unwrap();
        assert_eq!(v.id, "vidXYZ");
        assert_eq!(v.title, "Video Title");
        assert_eq!(v.duration, Some("PT4M13S".into()));
        assert_eq!(v.view_count, Some(12345));
        assert_eq!(v.published_at, Some("2024-06-01T12:00:00Z".into()));
    }

    // -----------------------------------------------------------------------
    // parse_playlist_item
    // -----------------------------------------------------------------------

    #[test]
    fn parse_playlist_item_with_content_details() {
        let json = serde_json::json!({
            "snippet": {
                "title": "Item",
                "videoOwnerChannelId": "UCowner",
                "videoOwnerChannelTitle": "Owner",
                "thumbnails": {},
                "publishedAt": "2024-01-01T00:00:00Z",
                "position": 3
            },
            "contentDetails": {"videoId": "vid123"}
        });
        let item = parse_playlist_item(&json).unwrap();
        assert_eq!(item.video_id, "vid123");
        assert_eq!(item.title, "Item");
        assert_eq!(item.channel_id, Some("UCowner".into()));
        assert_eq!(item.position, Some(3));
    }

    #[test]
    fn parse_playlist_item_with_resource_id() {
        let json = serde_json::json!({
            "snippet": {
                "title": "Item2",
                "resourceId": {"videoId": "vid456"},
                "thumbnails": {}
            }
        });
        let item = parse_playlist_item(&json).unwrap();
        assert_eq!(item.video_id, "vid456");
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
}
