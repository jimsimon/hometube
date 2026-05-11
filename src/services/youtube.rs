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
}

impl YoutubeClient {
    /// Construct a client from the API key stored in `app_config`.
    pub async fn from_db(pool: &SqlitePool) -> AppResult<Self> {
        let api_key = get_config_value(pool, KEY_YOUTUBE_API_KEY)
            .await?
            .ok_or_else(|| AppError::BadRequest("YouTube API key not configured".into()))?;
        Ok(Self {
            api_key,
            http: Client::new(),
            cache: Arc::new(Mutex::new(HashMap::new())),
        })
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
        let url = build_canonical_url(&format!("{API_BASE}{path}"), &full_query);

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
