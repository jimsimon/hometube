/**
 * Offline-download helpers.
 *
 * HomeTube stores downloaded videos in the browser's Cache Storage
 * (`caches.open(VIDEO_CACHE)`). We picked the Cache API over OPFS
 * because:
 *
 *   1. Service workers can read from `caches` directly. Reaching OPFS
 *      from a SW requires a `BroadcastChannel`/`SharedWorker` bridge
 *      with the page, which is fragile and isn't supported on Safari.
 *   2. The Cache API natively understands `Request`/`Response` objects,
 *      so the SW's `fetch` handler can do `caches.match(request)` and
 *      return the result directly.
 *   3. Cache responses are persisted across page reloads exactly like
 *      OPFS, and `navigator.storage.persist()` covers them too.
 *
 * The trade-off is no random-access I/O — but for full-file video
 * downloads the browser handles range requests automatically against a
 * cached `Response` body in modern Chromium, and we add a fallback
 * `Range`-aware path in the SW to keep Firefox/Safari working.
 *
 * Cache key shape: `https://<origin>/__hometube/offline/<videoId>/<quality>`
 * — a synthetic URL we never actually fetch. The SW's video-proxy
 * handler maps real `/api/...` URLs to this synthetic key before
 * looking up the cache.
 */
import type { ContinueWatchingItem, VideoMetadata } from "../types/index.js";

/** Name of the `caches` bucket used for downloaded video bodies. */
export const VIDEO_CACHE = "hometube-videos-v1";

/** localStorage key for the downloaded-video manifest (metadata index). */
const MANIFEST_KEY = "hometube-offline-manifest";

/** Per-video user preferences (e.g. audio-only mode). */
const PREFS_KEY_PREFIX = "hometube-video-prefs:";

export interface OfflineEntry {
  videoId: string;
  quality: string;
  /** Original URL used for the download (so the SW can match it). */
  sourceUrl: string;
  title: string | null;
  thumbnailUrl: string | null;
  channelTitle: string | null;
  durationSeconds: number | null;
  sizeBytes: number | null;
  downloadedAt: number;
}

interface OfflineManifest {
  version: 1;
  entries: OfflineEntry[];
}

/** Synthetic cache URL used as the storage key for a (video, quality). */
export function offlineCacheKey(videoId: string, quality: string): string {
  // Use the page origin so the URL is same-origin (caches don't allow
  // arbitrary URLs in some browsers).
  const origin = typeof location === "undefined" ? "http://localhost" : location.origin;
  return `${origin}/__hometube/offline/${encodeURIComponent(
    videoId,
  )}/${encodeURIComponent(quality)}`;
}

function readManifest(): OfflineManifest {
  try {
    const raw = localStorage.getItem(MANIFEST_KEY);
    if (!raw) return { version: 1, entries: [] };
    const parsed = JSON.parse(raw) as OfflineManifest;
    if (parsed.version !== 1 || !Array.isArray(parsed.entries)) {
      return { version: 1, entries: [] };
    }
    return parsed;
  } catch {
    return { version: 1, entries: [] };
  }
}

function writeManifest(manifest: OfflineManifest): void {
  localStorage.setItem(MANIFEST_KEY, JSON.stringify(manifest));
}

/**
 * Persist a video's bytes to the Cache API and append an entry to the
 * offline manifest. The caller is responsible for fetching the response
 * (via `fetch()` or Background Fetch) and passing the raw `Response` —
 * this keeps progress reporting in the caller's hands.
 */
export async function saveVideoToOpfs(
  videoId: string,
  quality: string,
  response: Response,
  meta: VideoMetadata,
  sourceUrl: string,
): Promise<OfflineEntry> {
  if (!("caches" in self)) {
    throw new Error("Cache Storage API is not available in this browser.");
  }
  const cache = await caches.open(VIDEO_CACHE);
  const key = offlineCacheKey(videoId, quality);

  // Clone the body into a Blob so we can read its size. We also stamp
  // a couple of headers so the SW can serve range requests sensibly.
  const blob = await response.blob();
  const cacheable = new Response(blob, {
    status: 200,
    headers: {
      "Content-Type": response.headers.get("Content-Type") ?? "video/mp4",
      "Content-Length": String(blob.size),
      "Accept-Ranges": "bytes",
      "X-HomeTube-Offline": "1",
    },
  });
  await cache.put(key, cacheable);

  const entry: OfflineEntry = {
    videoId,
    quality,
    sourceUrl,
    title: meta.title,
    thumbnailUrl: meta.thumbnail_url,
    channelTitle: meta.channel_title,
    durationSeconds: meta.duration_seconds,
    sizeBytes: blob.size,
    downloadedAt: Date.now(),
  };

  const manifest = readManifest();
  manifest.entries = manifest.entries
    .filter((e) => !(e.videoId === videoId && e.quality === quality))
    .concat(entry);
  writeManifest(manifest);

  return entry;
}

/**
 * Look up a downloaded video. Returns the cached `Response` (already
 * cloned for safe consumption) or `null` if it isn't present.
 */
export async function getOfflineVideoStream(
  videoId: string,
  quality: string,
): Promise<Response | null> {
  if (!("caches" in self)) return null;
  const cache = await caches.open(VIDEO_CACHE);
  const match = await cache.match(offlineCacheKey(videoId, quality));
  return match ? match.clone() : null;
}

/** List every downloaded video, newest first. */
export function listOfflineVideos(): OfflineEntry[] {
  const m = readManifest();
  return [...m.entries].sort((a, b) => b.downloadedAt - a.downloadedAt);
}

/** Remove a downloaded video from cache + manifest. */
export async function deleteOfflineVideo(videoId: string, quality: string): Promise<boolean> {
  let removed = false;
  if ("caches" in self) {
    const cache = await caches.open(VIDEO_CACHE);
    removed = await cache.delete(offlineCacheKey(videoId, quality));
  }
  const manifest = readManifest();
  const before = manifest.entries.length;
  manifest.entries = manifest.entries.filter(
    (e) => !(e.videoId === videoId && e.quality === quality),
  );
  if (manifest.entries.length !== before) {
    writeManifest(manifest);
    removed = true;
  }
  return removed;
}

/**
 * Browser-storage-quota helpers. Returns `null` when the browser
 * doesn't expose the Storage API (e.g. older Safari).
 */
export async function getStorageEstimate(): Promise<{
  usage: number;
  quota: number;
  percentUsed: number;
} | null> {
  if (!navigator.storage?.estimate) return null;
  const e = await navigator.storage.estimate();
  const usage = e.usage ?? 0;
  const quota = e.quota ?? 0;
  const percentUsed = quota > 0 ? (usage / quota) * 100 : 0;
  return { usage, quota, percentUsed };
}

/** Request persistent storage (best-effort; resolves to whether it was granted). */
export async function ensurePersistentStorage(): Promise<boolean> {
  if (!navigator.storage?.persist) return false;
  try {
    const already = await navigator.storage.persisted?.();
    if (already) return true;
    return await navigator.storage.persist();
  } catch {
    return false;
  }
}

// ---------------------------------------------------------------------
// Per-video preferences (audio-only mode toggle, etc.)
// ---------------------------------------------------------------------

export interface VideoPrefs {
  audioOnly?: boolean;
}

export function getVideoPrefs(videoId: string): VideoPrefs {
  try {
    const raw = localStorage.getItem(PREFS_KEY_PREFIX + videoId);
    if (!raw) return {};
    return JSON.parse(raw) as VideoPrefs;
  } catch {
    return {};
  }
}

export function setVideoPrefs(videoId: string, prefs: VideoPrefs): void {
  try {
    localStorage.setItem(PREFS_KEY_PREFIX + videoId, JSON.stringify(prefs));
  } catch {
    // ignore (private browsing, quota, etc.)
  }
}

// Helper used by the downloads UI to turn an entry into a continue-
// watching-style item suitable for `<hometube-video-card>`.
export function entryToCardItem(e: OfflineEntry): ContinueWatchingItem {
  return {
    video_id: e.videoId,
    video_title: e.title ?? e.videoId,
    video_thumbnail_url: e.thumbnailUrl,
    channel_title: e.channelTitle,
    duration_seconds: e.durationSeconds,
    progress_seconds: 0,
    last_watched_at: Math.floor(e.downloadedAt / 1000),
  };
}
