/**
 * HomeTube service worker.
 *
 * Built with `vite-plugin-pwa` in `injectManifest` mode — Workbox
 * generates a precache list and replaces `self.__WB_MANIFEST` with
 * it at build time.
 *
 * Responsibilities:
 *   1. Precache the app shell (index page, JS/CSS bundles, the
 *      offline fallback page) via Workbox's `precacheAndRoute`.
 *   2. When a download is requested, ask any open client whether it
 *      has the file in OPFS via the [`opfs-bridge`](./services/opfs-bridge.ts)
 *      message protocol, and serve the bytes back. If no client can
 *      answer, fall through to the network.
 *   3. Serve a friendly `/offline.html` fallback when an HTML
 *      navigation fails offline.
 *
 * OPFS-from-SW is impossible in the current spec — the SW can only
 * forward read requests to a window/worker that *does* have OPFS
 * access. When the user visits the page from a closed-PWA cold-start
 * with no other tab open, we degrade to "downloads unavailable until
 * a tab is open" — the original plan accepts this trade-off.
 */
/// <reference lib="webworker" />
import { precacheAndRoute } from "workbox-precaching";

declare const self: ServiceWorkerGlobalScope & {
  __WB_MANIFEST: { url: string; revision: string | null }[];
};

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
              n !== RUNTIME_CACHE &&
              !n.startsWith("workbox-precache-"),
          )
          .map((n) => caches.delete(n)),
      );
      await self.clients.claim();
    })(),
  );
});

interface OpfsExistsRequest {
  type: "opfs.exists";
  videoId: string;
  quality: string | null;
}

interface OpfsReadRequest {
  type: "opfs.read";
  videoId: string;
  quality: string;
  range?: [number, number];
}

interface OpfsExistsResponse {
  ok: true;
  type: "opfs.exists.response";
  has: boolean;
}

interface OpfsReadOkResponse {
  ok: true;
  type: "opfs.read.response";
  body: ArrayBuffer;
  contentType: string;
  contentLength: number;
  contentRange?: { start: number; end: number; total: number };
}

type OpfsResponse = OpfsExistsResponse | OpfsReadOkResponse | { ok: false };

/**
 * Send a single message to one client and return the first reply.
 * Times out at `timeoutMs` so a misbehaving page can't pin the SW.
 */
async function askClient(
  client: Client,
  message: OpfsExistsRequest | OpfsReadRequest,
  timeoutMs = 8000,
): Promise<OpfsResponse | null> {
  return new Promise((resolve) => {
    const channel = new MessageChannel();
    const timer = setTimeout(() => {
      channel.port1.close();
      resolve(null);
    }, timeoutMs);
    channel.port1.onmessage = (event) => {
      clearTimeout(timer);
      channel.port1.close();
      resolve(event.data as OpfsResponse);
    };
    try {
      client.postMessage(message, [channel.port2]);
    } catch {
      clearTimeout(timer);
      resolve(null);
    }
  });
}

async function findClientWithVideo(
  videoId: string,
  quality: string | null,
): Promise<Client | null> {
  const clients = await self.clients.matchAll({
    includeUncontrolled: true,
    type: "window",
  });
  for (const client of clients) {
    const reply = await askClient(client, {
      type: "opfs.exists",
      videoId,
      quality,
    });
    if (reply && reply.ok && "has" in reply && reply.has) {
      return client;
    }
  }
  return null;
}

function parseVideoRequest(
  url: URL,
  pathname: string,
): { videoId: string; quality: string | null } | null {
  if (pathname === "/api/proxy/video") {
    const videoId = url.searchParams.get("video_id");
    if (!videoId) return null;
    const quality = url.searchParams.get("quality") ?? url.searchParams.get("format");
    return { videoId, quality };
  }
  const m = /^\/api\/downloads\/([^/]+)\/stream$/.exec(pathname);
  if (m) {
    const videoId = decodeURIComponent(m[1] ?? "");
    if (!videoId) return null;
    return { videoId, quality: url.searchParams.get("quality") };
  }
  return null;
}

function parseRange(header: string | null): [number, number?] | null {
  if (!header) return null;
  const m = /bytes=(\d*)-(\d*)/.exec(header);
  if (!m) return null;
  const start = m[1] ? Number(m[1]) : 0;
  const end = m[2] ? Number(m[2]) : undefined;
  return end === undefined ? [start] : [start, end];
}

/**
 * Try to serve a downloaded video out of OPFS via a client bridge.
 * Returns `null` if no client can serve it.
 */
async function matchOfflineVideo(req: Request): Promise<Response | null> {
  const url = new URL(req.url);
  const parsed = parseVideoRequest(url, url.pathname);
  if (!parsed) return null;

  const client = await findClientWithVideo(parsed.videoId, parsed.quality);
  if (!client) return null;

  // We need a real quality string to issue the read. If the request
  // didn't specify one, ask the client to pick the first match.
  const range = parseRange(req.headers.get("Range"));
  const readReq: OpfsReadRequest = {
    type: "opfs.read",
    videoId: parsed.videoId,
    quality: parsed.quality ?? "",
    ...(range && range[1] !== undefined ? { range: [range[0], range[1]] as [number, number] } : {}),
  };
  // Empty quality → ask the client which one to use. Expanded
  // negotiation can come later; for now the client maps "" to the
  // first available file.
  if (!parsed.quality) {
    // Heuristic: prefer 720p, then 480p, then anything.
    for (const candidate of ["720p", "480p", "1080p", "360p"]) {
      const probe = await askClient(client, {
        type: "opfs.exists",
        videoId: parsed.videoId,
        quality: candidate,
      });
      if (probe && probe.ok && "has" in probe && probe.has) {
        readReq.quality = candidate;
        break;
      }
    }
  }
  if (!readReq.quality) return null;

  const reply = await askClient(client, readReq, 30_000);
  if (!reply || !reply.ok || reply.type !== "opfs.read.response") return null;

  if (reply.contentRange) {
    return new Response(reply.body, {
      status: 206,
      headers: {
        "Content-Type": reply.contentType,
        "Content-Length": String(reply.contentLength),
        "Content-Range": `bytes ${reply.contentRange.start}-${reply.contentRange.end}/${reply.contentRange.total}`,
        "Accept-Ranges": "bytes",
      },
    });
  }
  return new Response(reply.body, {
    status: 200,
    headers: {
      "Content-Type": reply.contentType,
      "Content-Length": String(reply.contentLength),
      "Accept-Ranges": "bytes",
    },
  });
}

self.addEventListener("fetch", (event: FetchEvent) => {
  const req = event.request;
  if (req.method !== "GET") return;

  const url = new URL(req.url);
  if (url.origin !== self.location.origin) return;

  // 1. OPFS-bridged offline lookup for downloaded videos.
  if (
    url.pathname === "/api/proxy/video" ||
    /^\/api\/downloads\/[^/]+\/stream$/.test(url.pathname)
  ) {
    event.respondWith(
      (async () => {
        try {
          const offline = await matchOfflineVideo(req);
          if (offline) return offline;
        } catch {
          // Fall through to the network.
        }
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
