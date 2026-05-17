// HomeTube Discovery Sidecar
//
// A lightweight HTTP service wrapping youtubei.js to provide YouTube
// content discovery (search, channel/playlist/video metadata) without
// using the official YouTube Data API v3.
//
// Endpoints return JSON matching HomeTube's internal Rust types so the
// backend can deserialize responses directly.

import { createServer } from "node:http";
import { Innertube, YTNodes } from "youtubei.js";

const PORT = parseInt(process.env.PORT || "3000", 10);

// Lazily initialised Innertube client. Re-created if it becomes stale.
let yt = null;

async function getClient() {
  if (!yt) {
    yt = await Innertube.create({
      lang: "en",
      location: "US",
      retrieve_player: false,
    });
  }
  return yt;
}

// ---------------------------------------------------------------------------
// Thumbnail helpers
// ---------------------------------------------------------------------------

/** Convert a youtubei.js Thumbnail[] array into the { size: { url, width, height } } map HomeTube expects. */
function mapThumbnails(thumbnails) {
  if (!thumbnails || !Array.isArray(thumbnails) || thumbnails.length === 0)
    return {};
  const out = {};
  // youtubei.js gives an array sorted by resolution. Map to named sizes.
  const sorted = [...thumbnails].sort(
    (a, b) => (a.width || 0) - (b.width || 0)
  );
  const names = ["default", "medium", "high", "standard", "maxres"];
  for (let i = 0; i < sorted.length && i < names.length; i++) {
    out[names[i]] = {
      url: sorted[i].url,
      width: sorted[i].width || null,
      height: sorted[i].height || null,
    };
  }
  return out;
}

/** Safely convert a Text node to a plain string. */
function textToString(text) {
  if (!text) return "";
  if (typeof text === "string") return text;
  if (typeof text.toString === "function") return text.toString();
  if (text.text) return text.text;
  return "";
}

/** Convert seconds to ISO 8601 duration (e.g. PT4M13S). */
function secondsToISO8601(seconds) {
  if (seconds == null || isNaN(seconds)) return null;
  const s = Math.floor(seconds);
  const h = Math.floor(s / 3600);
  const m = Math.floor((s % 3600) / 60);
  const sec = s % 60;
  let out = "PT";
  if (h > 0) out += `${h}H`;
  if (m > 0) out += `${m}M`;
  out += `${sec}S`;
  return out;
}

// ---------------------------------------------------------------------------
// Route handlers
// ---------------------------------------------------------------------------

/** GET /search?q=...&type=channel|playlist|video&maxResults=N */
async function handleSearch(url) {
  const q = url.searchParams.get("q");
  if (!q) return jsonError(400, "q parameter required");

  const type = url.searchParams.get("type") || "video";
  const maxResults = Math.min(
    parseInt(url.searchParams.get("maxResults") || "15", 10),
    50
  );

  const client = await getClient();
  const results = await client.search(q, { type });

  const items = [];

  // results may have .videos, .channels, .playlists depending on type
  // but we iterate results.results which has all mixed
  if (results.results) {
    for (const item of results.results) {
      if (items.length >= maxResults) break;

      if (
        type === "video" &&
        (item.is(YTNodes.Video) || item.is(YTNodes.CompactVideo))
      ) {
        items.push({
          kind: "video",
          id: item.id,
          title: textToString(item.title),
          description: textToString(item.description_snippet || item.snippets?.[0]?.text) || "",
          channel_id: item.author?.id || null,
          channel_title: item.author?.name || null,
          thumbnails: mapThumbnails(item.thumbnails),
          published_at: textToString(item.published) || null,
        });
      } else if (
        type === "channel" &&
        item.is(YTNodes.Channel)
      ) {
        items.push({
          kind: "channel",
          id: item.id,
          title: textToString(item.author?.name || item.title),
          description: textToString(item.description_snippet || item.description) || "",
          channel_id: item.id,
          channel_title: textToString(item.author?.name || item.title),
          thumbnails: mapThumbnails(item.author?.thumbnails || item.thumbnails),
          published_at: null,
        });
      } else if (
        type === "playlist" &&
        item.is(YTNodes.Playlist)
      ) {
        items.push({
          kind: "playlist",
          id: item.id,
          title: textToString(item.title),
          description: "",
          channel_id: item.author?.id || null,
          channel_title: item.author?.name || null,
          thumbnails: mapThumbnails(item.thumbnails),
          published_at: null,
        });
      }
    }
  }

  return jsonOk({ items });
}

/** GET /channels/:id */
async function handleGetChannel(channelId) {
  const client = await getClient();
  const channel = await client.getChannel(channelId);

  if (!channel) return jsonError(404, "channel not found");

  // Extract header info
  let title = "";
  let description = "";
  let thumbnails = [];
  let subscriberCount = null;
  let videoCount = null;

  if (channel.header) {
    if (channel.header.is(YTNodes.C4TabbedHeader)) {
      title = channel.header.author?.name || "";
      thumbnails = channel.header.author?.thumbnails || [];
      const subText = textToString(channel.header.subscriber_count);
      subscriberCount = parseCount(subText);
      const vidText = textToString(channel.header.videos_count);
      videoCount = parseCount(vidText);
    } else if (channel.header.is(YTNodes.PageHeader)) {
      title = textToString(channel.header.page_title) || "";
      const meta = channel.header.content?.metadata;
      if (meta) {
        thumbnails = meta.avatar?.image || [];
      }
    }
  }

  // Fallback to metadata
  if (!title && channel.metadata) {
    title = channel.metadata.title || "";
    description = channel.metadata.description || "";
  }

  // Build uploads playlist ID (UC... -> UU...)
  const uploadsPlaylistId = channelId.startsWith("UC")
    ? "UU" + channelId.slice(2)
    : null;

  return jsonOk({
    id: channelId,
    title,
    description,
    thumbnails: mapThumbnails(thumbnails),
    subscriber_count: subscriberCount,
    video_count: videoCount,
    uploads_playlist_id: uploadsPlaylistId,
  });
}

/** GET /playlists/:id */
async function handleGetPlaylist(playlistId) {
  const client = await getClient();
  const playlist = await client.getPlaylist(playlistId);

  if (!playlist) return jsonError(404, "playlist not found");

  const info = playlist.info;
  const header = playlist.header;

  let title = "";
  let description = "";
  let channelId = null;
  let channelTitle = null;
  let thumbnails = [];
  let itemCount = null;

  if (header) {
    if (header.is(YTNodes.PlaylistHeader)) {
      title = textToString(header.title);
      const statsTexts = header.stats?.map((s) => textToString(s)) || [];
      // First stat is usually "N videos"
      if (statsTexts.length > 0) {
        itemCount = parseCount(statsTexts[0]);
      }
      channelTitle = header.author?.name || null;
      channelId = header.author?.id || null;
    }
  }

  // Fallback to info
  if (!title && info) {
    title = info.title || "";
    description = info.description || "";
    channelTitle = info.author?.name || null;
    channelId = info.author?.id || null;
  }

  // Get thumbnails from first video if available
  if (playlist.items?.length > 0) {
    thumbnails = playlist.items[0].thumbnails || [];
  }

  return jsonOk({
    id: playlistId,
    title,
    description,
    channel_id: channelId,
    channel_title: channelTitle,
    thumbnails: mapThumbnails(thumbnails),
    item_count: itemCount,
  });
}

/** GET /videos/:id */
async function handleGetVideo(videoId) {
  const client = await getClient();
  const info = await client.getBasicInfo(videoId);

  if (!info || !info.basic_info) return jsonError(404, "video not found");

  const bi = info.basic_info;

  return jsonOk({
    id: bi.id || videoId,
    title: bi.title || "",
    description: bi.short_description || "",
    channel_id: bi.channel_id || null,
    channel_title: bi.author || null,
    thumbnails: mapThumbnails(bi.thumbnail),
    published_at: null, // getBasicInfo doesn't reliably return publish date
    duration: secondsToISO8601(bi.duration),
    view_count: bi.view_count ?? null,
  });
}

/** GET /channel-videos/:channelId?maxResults=N */
async function handleChannelVideos(channelId, url) {
  const maxResults = Math.min(
    parseInt(url.searchParams.get("maxResults") || "30", 10),
    50
  );

  const client = await getClient();
  const channel = await client.getChannel(channelId);
  if (!channel) return jsonError(404, "channel not found");

  // Get videos tab
  let videosTab;
  try {
    videosTab = await channel.getVideos();
  } catch {
    // Some channels may not have a videos tab
    return jsonOk({ items: [], next_page_token: null });
  }

  const items = [];
  if (videosTab?.videos) {
    for (const video of videosTab.videos) {
      if (items.length >= maxResults) break;
      items.push({
        video_id: video.id,
        title: textToString(video.title),
        channel_id: video.author?.id || channelId,
        channel_title: video.author?.name || null,
        thumbnails: mapThumbnails(video.thumbnails),
        published_at: textToString(video.published) || null,
        position: null,
      });
    }
  }

  // Channel video listings are single-page — callers (new-videos feed,
  // up-next, preview) only ever request one page with small maxResults.
  // youtubei.js continuations are stateful objects that can't be
  // serialized across HTTP requests, so we don't expose pagination here.
  return jsonOk({
    items,
    next_page_token: null,
  });
}

/** GET /playlist-items/:playlistId?maxResults=N
 *
 * Fetches ALL items from the playlist by looping through youtubei.js
 * continuations internally. The Rust consumer (`populate_playlist_videos`)
 * used to paginate via `next_page_token`, but youtubei.js continuation
 * objects are stateful and can't be serialized across HTTP requests.
 * Instead we gather everything server-side and return it in one response.
 *
 * The `maxResults` parameter caps the returned items (default 50, max 500).
 */
async function handlePlaylistItems(playlistId, url) {
  const maxResults = Math.min(
    parseInt(url.searchParams.get("maxResults") || "50", 10),
    500
  );

  const client = await getClient();
  let page = await client.getPlaylist(playlistId);
  if (!page) return jsonError(404, "playlist not found");

  const items = [];

  // Loop through all continuation pages to gather every item.
  while (page) {
    for (const item of page.items || []) {
      if (items.length >= maxResults) break;

      const videoId = item.id || item.video_id;
      if (!videoId) continue;

      items.push({
        video_id: videoId,
        title: textToString(item.title),
        channel_id: item.author?.id || null,
        channel_title: item.author?.name || null,
        thumbnails: mapThumbnails(item.thumbnails),
        published_at: null,
        position: item.index != null ? parseInt(textToString(item.index), 10) || null : null,
      });
    }

    if (items.length >= maxResults) break;

    if (page.has_continuation) {
      try {
        page = await page.getContinuation();
      } catch {
        break;
      }
    } else {
      break;
    }
  }

  return jsonOk({
    items,
    next_page_token: null,
  });
}

// ---------------------------------------------------------------------------
// Utility
// ---------------------------------------------------------------------------

/** Parse a human-readable count like "1.2M" or "10,000" into a number. */
function parseCount(text) {
  if (!text) return null;
  // Remove commas and whitespace
  const cleaned = text.replace(/[,\s]/g, "").toLowerCase();
  const match = cleaned.match(/^([\d.]+)\s*([kmb])?/);
  if (!match) return null;
  let num = parseFloat(match[1]);
  if (isNaN(num)) return null;
  const suffix = match[2];
  if (suffix === "k") num *= 1000;
  else if (suffix === "m") num *= 1000000;
  else if (suffix === "b") num *= 1000000000;
  return Math.round(num);
}

function jsonOk(data) {
  return { status: 200, body: JSON.stringify(data) };
}

function jsonError(status, message) {
  return { status, body: JSON.stringify({ error: message }) };
}

// ---------------------------------------------------------------------------
// Router
// ---------------------------------------------------------------------------

async function handleRequest(req) {
  const url = new URL(req.url, `http://localhost:${PORT}`);
  const path = url.pathname;
  const method = req.method;

  if (method !== "GET") return jsonError(405, "method not allowed");

  try {
    if (path === "/health") {
      return jsonOk({ status: "ok" });
    }

    if (path === "/search") {
      return await handleSearch(url);
    }

    // /channels/:id
    const channelMatch = path.match(/^\/channels\/([^/]+)$/);
    if (channelMatch) {
      return await handleGetChannel(decodeURIComponent(channelMatch[1]));
    }

    // /playlists/:id
    const playlistMatch = path.match(/^\/playlists\/([^/]+)$/);
    if (playlistMatch) {
      return await handleGetPlaylist(decodeURIComponent(playlistMatch[1]));
    }

    // /videos/:id
    const videoMatch = path.match(/^\/videos\/([^/]+)$/);
    if (videoMatch) {
      return await handleGetVideo(decodeURIComponent(videoMatch[1]));
    }

    // /channel-videos/:channelId
    const channelVideosMatch = path.match(/^\/channel-videos\/([^/]+)$/);
    if (channelVideosMatch) {
      return await handleChannelVideos(
        decodeURIComponent(channelVideosMatch[1]),
        url
      );
    }

    // /playlist-items/:playlistId
    const playlistItemsMatch = path.match(/^\/playlist-items\/([^/]+)$/);
    if (playlistItemsMatch) {
      return await handlePlaylistItems(
        decodeURIComponent(playlistItemsMatch[1]),
        url
      );
    }

    return jsonError(404, "not found");
  } catch (err) {
    console.error("Request failed:", path, err);

    // Re-create the client on errors (may be stale session)
    yt = null;

    const msg = (err.message || "unknown error").substring(0, 200);
    return jsonError(502, `upstream error: ${msg}`);
  }
}

// ---------------------------------------------------------------------------
// HTTP Server
// ---------------------------------------------------------------------------

const server = createServer(async (req, res) => {
  const result = await handleRequest(req);
  res.writeHead(result.status, { "Content-Type": "application/json" });
  res.end(result.body);
});

server.listen(PORT, () => {
  console.log(`HomeTube discovery sidecar listening on :${PORT}`);
});
