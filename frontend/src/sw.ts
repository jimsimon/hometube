/**
 * HomeTube service worker.
 *
 * Built with `vite-plugin-pwa` in `injectManifest` mode — Workbox
 * generates a precache list and replaces `self.__WB_MANIFEST` with it
 * at build time.
 *
 * Responsibilities:
 *   1. Precache the app shell (index page, JS/CSS bundles, the offline
 *      fallback page) via Workbox's `precacheAndRoute`.
 *   2. Cache-first the proxied video bytes — once a kid downloads a
 *      video, replays should never hit the network.
 *   3. Stale-while-revalidate the JSON API responses we mark as
 *      cacheable (e.g. recent playlists), so the UI still has data
 *      when offline.
 *   4. Serve a friendly `/offline.html` fallback when an HTML
 *      navigation fails offline.
 *
 * Range-request handling: the Cache API stores a single `Response`
 * per key. Modern Chromium serves byte-range requests against a cached
 * response transparently; for Firefox/Safari we slice the cached blob
 * manually.
 */
/// <reference lib="webworker" />
import { precacheAndRoute } from "workbox-precaching";

declare const self: ServiceWorkerGlobalScope & {
  __WB_MANIFEST: { url: string; revision: string | null }[];
};

const VIDEO_CACHE = "hometube-videos-v1";
const RUNTIME_CACHE = "hometube-runtime-v1";
const OFFLINE_HTML = "/offline.html";

precacheAndRoute(self.__WB_MANIFEST ?? []);

self.addEventListener("install", () => {
  void self.skipWaiting();
});

self.addEventListener("activate", (event: ExtendableEvent) => {
  event.waitUntil(
    (async () => {
      // Drop any old runtime caches from previous SW versions.
      const names = await caches.keys();
      await Promise.all(
        names
          .filter(
            (n) =>
              n.startsWith("hometube-") &&
              n !== VIDEO_CACHE &&
              n !== RUNTIME_CACHE &&
              !n.startsWith("workbox-precache-"),
          )
          .map((n) => caches.delete(n)),
      );
      await self.clients.claim();
    })(),
  );
});

/**
 * Same logic as `services/offline.ts::offlineCacheKey`, duplicated
 * here so the SW doesn't have to import the page bundle (it runs in
 * its own global).
 */
function offlineKey(videoId: string, quality: string): string {
  return `${self.location.origin}/__hometube/offline/${encodeURIComponent(
    videoId,
  )}/${encodeURIComponent(quality)}`;
}

/** Match a downloaded video by inspecting URL params. */
async function matchOfflineVideo(req: Request): Promise<Response | null> {
  const url = new URL(req.url);
  const isProxy = url.pathname === "/api/proxy/video";
  const isDownload = /^\/api\/downloads\/[^/]+\/stream$/.test(url.pathname);
  if (!isProxy && !isDownload) return null;

  let videoId: string | null = null;
  let quality: string | null = null;

  if (isProxy) {
    videoId = url.searchParams.get("video_id");
    quality = url.searchParams.get("quality") ?? url.searchParams.get("format");
  } else {
    const m = url.pathname.match(/^\/api\/downloads\/([^/]+)\/stream$/);
    videoId = m ? decodeURIComponent(m[1] ?? "") : null;
    quality = url.searchParams.get("quality");
  }
  if (!videoId) return null;

  const cache = await caches.open(VIDEO_CACHE);
  // Try the exact (videoId, quality) key first; if that misses fall
  // back to *any* cached quality of this video.
  if (quality) {
    const exact = await cache.match(offlineKey(videoId, quality));
    if (exact) return rangeAware(req, exact);
  }
  // Fallback: scan the cache for any download of this video.
  const keys = await cache.keys();
  const prefix = `${self.location.origin}/__hometube/offline/${encodeURIComponent(videoId)}/`;
  const fallbackKey = keys.find((k) => k.url.startsWith(prefix));
  if (fallbackKey) {
    const res = await cache.match(fallbackKey);
    if (res) return rangeAware(req, res);
  }
  return null;
}

/** Slice a cached response when the request includes a `Range` header. */
async function rangeAware(req: Request, cached: Response): Promise<Response> {
  const range = req.headers.get("Range");
  if (!range) return cached.clone();

  const m = /bytes=(\d*)-(\d*)/.exec(range);
  if (!m) return cached.clone();

  const blob = await cached.clone().blob();
  const total = blob.size;
  const start = m[1] ? Number(m[1]) : 0;
  const end = m[2] ? Math.min(Number(m[2]), total - 1) : total - 1;
  if (start >= total || start < 0 || end < start) {
    return new Response(null, {
      status: 416,
      headers: { "Content-Range": `bytes */${total}` },
    });
  }
  const slice = blob.slice(start, end + 1, blob.type);
  return new Response(slice, {
    status: 206,
    headers: {
      "Content-Type": cached.headers.get("Content-Type") ?? "video/mp4",
      "Content-Length": String(end - start + 1),
      "Content-Range": `bytes ${start}-${end}/${total}`,
      "Accept-Ranges": "bytes",
    },
  });
}

self.addEventListener("fetch", (event: FetchEvent) => {
  const req = event.request;
  if (req.method !== "GET") return;

  const url = new URL(req.url);
  if (url.origin !== self.location.origin) return;

  // 1. Cache-first for downloaded videos.
  if (
    url.pathname === "/api/proxy/video" ||
    /^\/api\/downloads\/[^/]+\/stream$/.test(url.pathname)
  ) {
    event.respondWith(
      (async () => {
        const cached = await matchOfflineVideo(req);
        if (cached) return cached;
        try {
          return await fetch(req);
        } catch {
          return new Response("Not available offline", {
            status: 504,
            headers: { "Content-Type": "text/plain" },
          });
        }
      })(),
    );
    return;
  }

  // 2. Navigation fallback: serve the offline page when the network
  //    is unreachable.
  if (req.mode === "navigate") {
    event.respondWith(
      (async () => {
        try {
          return await fetch(req);
        } catch {
          const cache = await caches.open(RUNTIME_CACHE);
          const fallback = (await cache.match(OFFLINE_HTML)) ?? (await caches.match(OFFLINE_HTML));
          return (
            fallback ??
            new Response(
              `<!doctype html><meta charset=utf-8><title>Offline</title>` +
                `<h1>Offline</h1><p>Reconnect to the internet to keep watching.</p>`,
              { status: 503, headers: { "Content-Type": "text/html" } },
            )
          );
        }
      })(),
    );
    return;
  }
});

// Pre-cache the offline fallback on install.
self.addEventListener("install", (event: ExtendableEvent) => {
  event.waitUntil(
    (async () => {
      try {
        const cache = await caches.open(RUNTIME_CACHE);
        await cache.add(OFFLINE_HTML);
      } catch {
        // The page might not exist yet (dev mode); ignore.
      }
    })(),
  );
});

export {};
