/**
 * Offline-download facade.
 *
 * Storage moved from Cache Storage (`caches.open(VIDEO_CACHE)`) to
 * the Origin Private File System in T-11 of the follow-up plan. The
 * original public surface is preserved here so existing callers keep
 * working without churn — every helper just delegates to
 * [`./offline-opfs.ts`].
 *
 * Why OPFS:
 *   1. Downloads stream straight to disk (`pipeTo`) instead of
 *      buffering the whole video in RAM.
 *   2. Per-file size accounting is exact via `FileSystemFileHandle`.
 *   3. Random-access seeks are cheap, so the service-worker bridge
 *      can serve byte-range requests without slicing a Response blob.
 *
 * Service workers can't reach OPFS directly; the `opfs-bridge.ts`
 * companion module wires up a `postMessage` channel so an open page
 * proxies the read on the SW's behalf. When no page is open we fall
 * through to the network — the original plan accepts this kind of
 * graceful degradation.
 */
import {
  deleteOfflineVideo as deleteOpfs,
  ensurePersistentStorage as ensureOpfs,
  entryToCardItem as entryToCardItemOpfs,
  getOfflineVideoStream as getOpfsStream,
  getStorageEstimate as getOpfsEstimate,
  getVideoPrefs as getPrefsOpfs,
  hasOfflineVideo as hasOpfs,
  isOpfsSupported,
  listOfflineVideos as listOpfs,
  OPFS_VIDEO_DIR,
  type OfflineEntry,
  saveVideoToOpfs as saveOpfs,
  setVideoPrefs as setPrefsOpfs,
  type VideoPrefs,
} from "./offline-opfs.js";
import type { ContinueWatchingItem, VideoMetadata } from "../types/index.js";

export type { OfflineEntry, VideoPrefs };

/**
 * Legacy Cache Storage bucket name. Retained as an export because
 * the migration shim and a couple of older tests still reference it,
 * and because some user agents may still have leftover entries from
 * pre-OPFS builds.
 */
export const VIDEO_CACHE = "hometube-videos-v1";

/** Public OPFS root directory used for downloaded videos. */
export { OPFS_VIDEO_DIR, isOpfsSupported };

/** Synthetic cache URL kept for backward compatibility. */
export function offlineCacheKey(videoId: string, quality: string): string {
  const origin = typeof location === "undefined" ? "http://localhost" : location.origin;
  return `${origin}/__hometube/offline/${encodeURIComponent(
    videoId,
  )}/${encodeURIComponent(quality)}`;
}

export async function saveVideoToOpfs(
  videoId: string,
  quality: string,
  response: Response,
  meta: VideoMetadata,
  sourceUrl: string,
): Promise<OfflineEntry> {
  return saveOpfs(videoId, quality, response, meta, sourceUrl);
}

export async function getOfflineVideoStream(
  videoId: string,
  quality: string,
): Promise<Response | null> {
  return getOpfsStream(videoId, quality);
}

export async function listOfflineVideos(): Promise<OfflineEntry[]> {
  return listOpfs();
}

export async function deleteOfflineVideo(videoId: string, quality: string): Promise<boolean> {
  return deleteOpfs(videoId, quality);
}

export function getStorageEstimate(): Promise<{
  usage: number;
  quota: number;
  percentUsed: number;
} | null> {
  return getOpfsEstimate();
}

export function ensurePersistentStorage(): Promise<boolean> {
  return ensureOpfs();
}

export function getVideoPrefs(videoId: string): VideoPrefs {
  return getPrefsOpfs(videoId);
}

export function setVideoPrefs(videoId: string, prefs: VideoPrefs): void {
  setPrefsOpfs(videoId, prefs);
}

export function entryToCardItem(e: OfflineEntry): ContinueWatchingItem {
  return entryToCardItemOpfs(e);
}

export async function hasOfflineVideo(videoId: string, quality: string | null): Promise<boolean> {
  return hasOpfs(videoId, quality);
}

/**
 * One-shot migration from the old Cache-Storage layout into OPFS.
 * Walks every entry in `caches.open(VIDEO_CACHE)`, copies the body
 * into OPFS via [`saveVideoToOpfs`], and deletes the old cache once
 * all entries land. Idempotent and gated by `localStorage` so it
 * runs at most once per browser profile.
 *
 * Returns the number of entries migrated. `0` is the steady-state.
 */
export async function migrateCacheStorageToOpfs(): Promise<number> {
  const FLAG = "hometube-opfs-migrated";
  if (typeof localStorage === "undefined" || typeof caches === "undefined") {
    return 0;
  }
  if (localStorage.getItem(FLAG) === "1") return 0;
  if (!isOpfsSupported()) return 0;

  let migrated = 0;
  try {
    const has = await caches.has(VIDEO_CACHE);
    if (!has) {
      localStorage.setItem(FLAG, "1");
      return 0;
    }
    const cache = await caches.open(VIDEO_CACHE);
    const keys = await cache.keys();
    for (const req of keys) {
      const url = new URL(req.url);
      // Old layout: /__hometube/offline/<videoId>/<quality>.
      const m = /\/__hometube\/offline\/([^/]+)\/([^/]+)$/.exec(url.pathname);
      if (!m) continue;
      const videoId = decodeURIComponent(m[1] ?? "");
      const quality = decodeURIComponent(m[2] ?? "");
      const res = await cache.match(req);
      if (!res) continue;
      try {
        // We don't have the original VideoMetadata here — fall back
        // to placeholder values; the user can re-fetch metadata at
        // playback time.
        await saveOpfs(
          videoId,
          quality,
          res,
          {
            id: videoId,
            title: videoId,
            channel_id: null,
            channel_title: null,
            duration_seconds: null,
            thumbnail_url: null,
          } as VideoMetadata,
          req.url,
        );
        migrated++;
      } catch {
        // Skip individual failures so one bad entry doesn't block
        // the rest of the migration.
      }
    }
    await caches.delete(VIDEO_CACHE);
    localStorage.setItem(FLAG, "1");
  } catch {
    // Migration is best-effort; clear the flag so we retry next load.
  }
  return migrated;
}
