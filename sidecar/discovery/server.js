// HomeTube Discovery Sidecar
//
// A lightweight HTTP service wrapping youtubei.js to provide YouTube
// content discovery (search, channel/video metadata) without using
// the official YouTube Data API v3.
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
    });
  }
  return yt;
}

// ---------------------------------------------------------------------------
// Thumbnail helpers
// ---------------------------------------------------------------------------

/**
 * Promote a protocol-relative URL (`//host/path`) to an explicit
 * `https://` URL. youtubei.js returns channel avatar thumbnails in
 * protocol-relative form; HomeTube stores and validates these URLs
 * server-side (the allowlist add rejects anything that isn't an
 * explicit `https://` YouTube host), so we normalise at the source.
 */
function normalizeThumbnailUrl(url) {
  if (typeof url !== "string") return url;
  return url.startsWith("//") ? `https:${url}` : url;
}

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
      url: normalizeThumbnailUrl(sorted[i].url),
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

/** GET /search?q=...&type=channel|video&maxResults=N */
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

  // results may have .videos or .channels depending on type, but we
  // iterate results.results which has all mixed.
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

  return jsonOk({
    id: channelId,
    title,
    description,
    thumbnails: mapThumbnails(thumbnails),
    subscriber_count: subscriberCount,
    video_count: videoCount,
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

  // Try the legacy .videos getter first (returns Video/GridVideo nodes).
  if (videosTab?.videos?.length > 0) {
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

  // YouTube now returns LockupView nodes inside RichItem containers on
  // the videos tab. When .videos is empty, extract from the tab content.
  // Note: uses `channel` from getChannel() above for metadata fallback.
  if (items.length === 0 && videosTab?.current_tab?.content?.contents) {
    for (const entry of videosTab.current_tab.content.contents) {
      if (items.length >= maxResults) break;
      const lockup = entry.content || entry;
      if (lockup.type !== "LockupView" || lockup.content_type !== "VIDEO") continue;

      const videoId = lockup.content_id;
      if (!videoId) continue;

      const title = textToString(lockup.metadata?.title);
      const thumbnails = lockup.content_image?.image || [];

      // Metadata rows contain "views • time ago" style parts.
      let publishedAt = null;
      const metaRows = lockup.metadata?.metadata?.metadata_rows;
      if (metaRows?.length > 0) {
        const parts = metaRows[0].metadata_parts;
        if (parts?.length >= 2) {
          publishedAt = textToString(parts[1].text) || null;
        }
      }

      items.push({
        video_id: videoId,
        title,
        channel_id: channelId,
        channel_title: channel.metadata?.title || null,
        thumbnails: mapThumbnails(thumbnails),
        published_at: publishedAt,
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

    return jsonError(404, "not found");
  } catch (err) {
    console.error("Request failed:", path, err);

    // Only cycle the `Innertube` client when the error suggests the
    // anonymous session itself is the problem (auth, consent
    // interstitial, IP-bound rate limit, dropped TLS session). For
    // ordinary application errors (404s, parse errors, "not found",
    // missing fields) we keep the session intact — a long-lived
    // visitorData looks more like a real user than one that churns
    // on every transient hiccup, which matters for YouTube's
    // anti-bot signals.
    const msg = (err.message || "").toString();
    // Patterns suggesting the anonymous session / network path is the
    // problem, not the application logic. Covers:
    //   - bot-check interstitials: "Sign in to confirm…", "consent.youtube.com"
    //   - InnerTube rate limits: "429", "Status code 429", "Rate limited"
    //   - auth surfaces: "unauthorized" / "unauthenticated", "forbidden",
    //     "Status code 401/403"
    //   - undici / Node 20 transient TLS / DNS: "aborted", "fetch failed",
    //     "network error", "ECONNRESET", "ETIMEDOUT", "EAI_AGAIN", "TLS",
    //     "certificate"
    //
    // Whole-word tokens carry `\b` anchors so substrings like "controls"
    // can't trigger recycling on the embedded "TLS"/"aborted"-ish fragments
    // that aren't actually session-related. `unauthor` is intentionally
    // a partial because it covers both "unauthorized" and "unauthenticated"
    // and there's no realistic English word with that as a non-prefix
    // substring.
    const sessionLikely = new RegExp(
      [
        /\bsign[- ]?in\b/.source,
        /\bconsent(?:ed|ing|s)?\b/.source,
        /\bstatus code 429\b/.source,
        /\btoo many requests\b/.source,
        /\brate[- ]?limit(?:ed|ing|s)?\b/.source,
        /\bstatus code 40[13]\b/.source,
        /\bunauth/.source,
        /\bforbidden\b/.source,
        /\baborted\b/.source,
        /\bfetch failed\b/.source,
        /\bnetwork error\b/.source,
        /\bECONNRESET\b/.source,
        /\bETIMEDOUT\b/.source,
        /\bEAI_AGAIN\b/.source,
        /\bcertificate\b/.source,
        /\bTLS\b/.source,
      ].join("|"),
      "i"
    ).test(msg);
    if (sessionLikely) {
      yt = null;
    }

    const truncated = msg.substring(0, 200) || "unknown error";
    return jsonError(502, `upstream error: ${truncated}`);
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
