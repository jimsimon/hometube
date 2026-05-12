/**
 * Unit tests for the offline-downloads service.
 *
 * Vitest runs these in real Chromium via `@vitest/browser-playwright`,
 * so OPFS (`navigator.storage.getDirectory`) and `localStorage` are
 * available.
 */
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import {
  deleteOfflineVideo,
  ensurePersistentStorage,
  entryToCardItem,
  getOfflineVideoStream,
  getStorageEstimate,
  getVideoPrefs,
  hasOfflineVideo,
  isOpfsSupported,
  listOfflineVideos,
  offlineCacheKey,
  OPFS_VIDEO_DIR,
  saveVideoToOpfs,
  setVideoPrefs,
  VIDEO_CACHE,
} from "./offline.js";
import type { VideoMetadata } from "../types/index.js";

const META: VideoMetadata = {
  id: "abc",
  title: "A test",
  channel_id: "chan",
  channel_title: "Channel",
  duration_seconds: 42,
  thumbnail_url: "https://example.com/thumb.jpg",
};

async function clearOpfs(): Promise<void> {
  if (!isOpfsSupported()) return;
  try {
    const root = await navigator.storage.getDirectory();
    await root.removeEntry(OPFS_VIDEO_DIR, { recursive: true });
  } catch {
    // Directory may not exist; ignore.
  }
}

async function clearAll(): Promise<void> {
  await clearOpfs();
  if ("caches" in self) {
    await caches.delete(VIDEO_CACHE);
  }
  localStorage.clear();
}

describe("offline service", () => {
  beforeEach(async () => {
    await clearAll();
  });

  afterEach(async () => {
    await clearAll();
    vi.restoreAllMocks();
  });

  it("produces a same-origin cache key", () => {
    const k = offlineCacheKey("abc", "720p");
    expect(k.startsWith(location.origin)).toBe(true);
    expect(k).toContain("abc");
    expect(k).toContain("720p");
  });

  it("save → list → get round-trip via OPFS", async () => {
    const blob = new Blob(["hello world"], { type: "video/mp4" });
    const res = new Response(blob, {
      status: 200,
      headers: { "Content-Type": "video/mp4" },
    });
    const entry = await saveVideoToOpfs("abc", "720p", res, META, "/api/x");
    expect(entry.videoId).toBe("abc");
    expect(entry.sizeBytes).toBeGreaterThan(0);

    const list = await listOfflineVideos();
    expect(list).toHaveLength(1);
    expect(list[0]?.title).toBe("A test");

    const cached = await getOfflineVideoStream("abc", "720p");
    expect(cached).not.toBeNull();
    const text = await cached!.text();
    expect(text).toBe("hello world");

    expect(await hasOfflineVideo("abc", "720p")).toBe(true);
    expect(await hasOfflineVideo("abc", null)).toBe(true);
    expect(await hasOfflineVideo("nope", "720p")).toBe(false);
  });

  it("returns null for missing offline videos", async () => {
    const cached = await getOfflineVideoStream("nope", "720p");
    expect(cached).toBeNull();
  });

  it("deleteOfflineVideo removes from OPFS and manifest", async () => {
    const res = new Response(new Blob(["x"]), { status: 200 });
    await saveVideoToOpfs("abc", "480p", res, META, "/api/x");
    expect(await listOfflineVideos()).toHaveLength(1);

    const removed = await deleteOfflineVideo("abc", "480p");
    expect(removed).toBe(true);
    expect(await listOfflineVideos()).toHaveLength(0);
    expect(await getOfflineVideoStream("abc", "480p")).toBeNull();
    expect(await hasOfflineVideo("abc", "480p")).toBe(false);
  });

  it("saving the same (video, quality) replaces the previous entry", async () => {
    await saveVideoToOpfs(
      "abc",
      "720p",
      new Response(new Blob(["v1"]), { status: 200 }),
      META,
      "/api/x",
    );
    await saveVideoToOpfs(
      "abc",
      "720p",
      new Response(new Blob(["v2"]), { status: 200 }),
      META,
      "/api/x",
    );
    expect(await listOfflineVideos()).toHaveLength(1);
    const cached = await getOfflineVideoStream("abc", "720p");
    expect(await cached!.text()).toBe("v2");
  });

  it("per-video prefs round-trip through localStorage", () => {
    expect(getVideoPrefs("abc")).toEqual({});
    setVideoPrefs("abc", { audioOnly: true });
    expect(getVideoPrefs("abc")).toEqual({ audioOnly: true });
  });

  it("getStorageEstimate returns null when the API is missing", async () => {
    const real = navigator.storage;
    Object.defineProperty(navigator, "storage", {
      configurable: true,
      value: {},
    });
    try {
      expect(await getStorageEstimate()).toBeNull();
    } finally {
      Object.defineProperty(navigator, "storage", {
        configurable: true,
        value: real,
      });
    }
  });

  it("getStorageEstimate computes percentUsed", async () => {
    if (!navigator.storage?.estimate) return;
    const est = await getStorageEstimate();
    expect(est).not.toBeNull();
    expect(est!.percentUsed).toBeGreaterThanOrEqual(0);
  });

  it("ensurePersistentStorage handles a missing API gracefully", async () => {
    const real = navigator.storage;
    Object.defineProperty(navigator, "storage", {
      configurable: true,
      value: {},
    });
    try {
      expect(await ensurePersistentStorage()).toBe(false);
    } finally {
      Object.defineProperty(navigator, "storage", {
        configurable: true,
        value: real,
      });
    }
  });

  it("entryToCardItem maps fields onto the continue-watching shape", () => {
    const card = entryToCardItem({
      videoId: "abc",
      quality: "720p",
      sourceUrl: "/api/x",
      title: "T",
      thumbnailUrl: null,
      channelTitle: null,
      durationSeconds: 12,
      sizeBytes: 1,
      downloadedAt: 0,
    });
    expect(card.video_id).toBe("abc");
    expect(card.video_title).toBe("T");
    expect(card.duration_seconds).toBe(12);
    expect(card.progress_seconds).toBe(0);
  });
});
