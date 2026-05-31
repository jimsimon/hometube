/**
 * OPFS-backed offline-download service.
 *
 * Replaces the original Cache-API implementation. The Origin Private
 * File System (`navigator.storage.getDirectory()`) gives us:
 *
 *   1. **Streamed writes.** `FileSystemWritableFileStream.pipeTo(body)`
 *      writes to disk as the bytes arrive, instead of buffering the
 *      whole video in RAM the way `cache.put(req, res)` does.
 *   2. **Per-file size accounting.** `FileSystemFileHandle.getFile()`
 *      returns a `File` whose `.size` is exact, so the downloads UI
 *      can show real bytes without bookkeeping.
 *   3. **Random-access seeks.** `FileSystemSyncAccessHandle.read()`
 *      (in workers) or `File.slice()` (here, in the page context)
 *      both let us serve range requests without slicing a Response
 *      blob in memory.
 *
 * The trade-off: service workers cannot reach OPFS directly. The
 * companion `opfs-bridge.ts` module wires up a `postMessage` protocol
 * so every open tab/window acts as an OPFS proxy for the SW. When no
 * client is open the SW falls through to the network (matching the
 * "graceful degradation" the plan calls out).
 *
 * Layout in OPFS:
 *
 *   /hometube-videos/<videoId>/<quality>.mp4   — raw bytes
 *   /hometube-videos/__index.json              — manifest (metadata)
 *
 * The `__index.json` shadow lives inside OPFS (instead of
 * `localStorage`) so the manifest survives "clear site data" only
 * when OPFS does, which keeps the two stores in lockstep.
 */
import type { ContinueWatchingItem, VideoMetadata } from "../types/index.js";

/**
 * Top-level OPFS directory containing every download. Older builds
 * stored bodies in `caches.open(VIDEO_CACHE)`; the migration in
 * `sw-register.ts` copies them in here on first load.
 */
export const OPFS_VIDEO_DIR = "hometube-videos";

/** Manifest filename inside [`OPFS_VIDEO_DIR`]. */
const INDEX_FILENAME = "__index.json";

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
  version: 2;
  entries: OfflineEntry[];
}

/**
 * Capability check for OPFS. False on older Safari and in Safari
 * Private Browsing. Callers should fall back to "downloads
 * unavailable" when this returns false.
 */
export function isOpfsSupported(): boolean {
  return Boolean(
    typeof navigator !== "undefined" &&
    navigator.storage &&
    typeof navigator.storage.getDirectory === "function",
  );
}

async function rootDir(): Promise<FileSystemDirectoryHandle> {
  if (!isOpfsSupported()) {
    throw new Error("OPFS is not available in this browser.");
  }
  return await navigator.storage.getDirectory();
}

async function videosDir(create = false): Promise<FileSystemDirectoryHandle> {
  const root = await rootDir();
  return await root.getDirectoryHandle(OPFS_VIDEO_DIR, { create });
}

async function videoSubdir(videoId: string, create = false): Promise<FileSystemDirectoryHandle> {
  const dir = await videosDir(create);
  return await dir.getDirectoryHandle(videoId, { create });
}

function qualityFilename(quality: string): string {
  // Allow only a small whitelist of characters in the on-disk name.
  // Anything fancy gets percent-encoded the same way `encodeURIComponent`
  // would, so the filename round-trips for any quality string.
  return `${encodeURIComponent(quality)}.mp4`;
}

async function readManifest(): Promise<OfflineManifest> {
  if (!isOpfsSupported()) return { version: 2, entries: [] };
  try {
    const dir = await videosDir(true);
    const handle = await dir.getFileHandle(INDEX_FILENAME, { create: false });
    const file = await handle.getFile();
    const text = await file.text();
    if (!text) return { version: 2, entries: [] };
    const parsed = JSON.parse(text) as OfflineManifest;
    if (parsed.version !== 2 || !Array.isArray(parsed.entries)) {
      return { version: 2, entries: [] };
    }
    return parsed;
  } catch {
    return { version: 2, entries: [] };
  }
}

async function writeManifest(manifest: OfflineManifest): Promise<void> {
  const dir = await videosDir(true);
  const handle = await dir.getFileHandle(INDEX_FILENAME, { create: true });
  const writable = await handle.createWritable();
  await writable.write(JSON.stringify(manifest));
  await writable.close();
}

/**
 * Persist a video's bytes to OPFS and append/replace its entry in the
 * offline manifest. The body is streamed to disk via
 * `WritableStream.pipeTo`, so the entire response is never held in
 * RAM.
 */
export async function saveVideoToOpfs(
  videoId: string,
  quality: string,
  response: Response,
  meta: VideoMetadata,
  sourceUrl: string,
): Promise<OfflineEntry> {
  if (!isOpfsSupported()) {
    throw new Error("OPFS is not available in this browser.");
  }
  if (!response.body) {
    throw new Error("Response has no body to download.");
  }

  const dir = await videoSubdir(videoId, true);
  const filename = qualityFilename(quality);
  const fileHandle = await dir.getFileHandle(filename, {
    create: true,
  });
  const writable = await fileHandle.createWritable();
  // Streamed pipe — tap the response body directly into the file.
  // On network failure the partial file is removed so we don't leak
  // storage on interrupted downloads.
  try {
    await response.body.pipeTo(writable);
  } catch (err) {
    try {
      await dir.removeEntry(filename);
    } catch {
      // Best-effort cleanup; the file may not exist if pipeTo failed
      // before any bytes were written.
    }
    throw err;
  }

  const written = await fileHandle.getFile();

  const entry: OfflineEntry = {
    videoId,
    quality,
    sourceUrl,
    title: meta.title,
    thumbnailUrl: meta.thumbnail_url,
    channelTitle: meta.channel_title,
    durationSeconds: meta.duration_seconds,
    sizeBytes: written.size,
    downloadedAt: Date.now(),
  };

  const manifest = await readManifest();
  manifest.entries = manifest.entries
    .filter((e) => !(e.videoId === videoId && e.quality === quality))
    .concat(entry);
  await writeManifest(manifest);

  return entry;
}

/**
 * Look up a downloaded video. Returns a `Response` constructed from
 * the on-disk file, with `Content-Type` and `Content-Length` headers
 * filled in. Returns `null` when the file isn't present.
 *
 * For ranged responses, the caller (typically the service worker
 * bridge) is responsible for slicing — this function always returns
 * the whole file as a 200.
 */
export async function getOfflineVideoStream(
  videoId: string,
  quality: string,
): Promise<Response | null> {
  if (!isOpfsSupported()) return null;
  try {
    const dir = await videoSubdir(videoId);
    const fileHandle = await dir.getFileHandle(qualityFilename(quality));
    const file = await fileHandle.getFile();
    return new Response(file, {
      status: 200,
      headers: {
        "Content-Type": file.type || "video/mp4",
        "Content-Length": String(file.size),
        "Accept-Ranges": "bytes",
        "X-HomeTube-Offline": "1",
      },
    });
  } catch {
    return null;
  }
}

/**
 * Read a byte range from a downloaded video. Returns the slice + the
 * total file size, or `null` if the file isn't present.
 */
export async function getOfflineVideoRange(
  videoId: string,
  quality: string,
  start: number,
  end: number,
): Promise<{ slice: Blob; total: number; contentType: string } | null> {
  if (!isOpfsSupported()) return null;
  try {
    const dir = await videoSubdir(videoId);
    const fileHandle = await dir.getFileHandle(qualityFilename(quality));
    const file = await fileHandle.getFile();
    const total = file.size;
    const safeStart = Math.max(0, Math.min(start, total - 1));
    const safeEnd = Math.max(safeStart, Math.min(end, total - 1));
    return {
      slice: file.slice(safeStart, safeEnd + 1, file.type || "video/mp4"),
      total,
      contentType: file.type || "video/mp4",
    };
  } catch {
    return null;
  }
}

/** Does this client have the requested (videoId, quality) pair? */
export async function hasOfflineVideo(videoId: string, quality: string | null): Promise<boolean> {
  if (!isOpfsSupported()) return false;
  try {
    const dir = await videoSubdir(videoId);
    if (quality) {
      await dir.getFileHandle(qualityFilename(quality));
      return true;
    }
    // Any quality is acceptable.
    for await (const _ of dir.values()) {
      return true;
    }
    return false;
  } catch {
    return false;
  }
}

/** List every downloaded video, newest first. */
export async function listOfflineVideos(): Promise<OfflineEntry[]> {
  const m = await readManifest();
  return [...m.entries].sort((a, b) => b.downloadedAt - a.downloadedAt);
}

/** Remove a downloaded video from OPFS + manifest. */
export async function deleteOfflineVideo(videoId: string, quality: string): Promise<boolean> {
  let removed = false;
  if (isOpfsSupported()) {
    try {
      const dir = await videoSubdir(videoId);
      await dir.removeEntry(qualityFilename(quality));
      removed = true;
      // If the per-video subdir is now empty, drop it too.
      let stillHasFiles = false;
      for await (const _ of dir.values()) {
        stillHasFiles = true;
        break;
      }
      if (!stillHasFiles) {
        const parent = await videosDir();
        try {
          await parent.removeEntry(videoId);
        } catch {
          // ignore — empty-dir removal is best-effort.
        }
      }
    } catch {
      // ignore — file may not exist; manifest update below still
      // catches the case where the row leaked.
    }
  }
  const manifest = await readManifest();
  const before = manifest.entries.length;
  manifest.entries = manifest.entries.filter(
    (e) => !(e.videoId === videoId && e.quality === quality),
  );
  if (manifest.entries.length !== before) {
    await writeManifest(manifest);
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
    published_at: null,
  };
}
