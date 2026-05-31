/**
 * Unit tests for the OPFS service-worker bridge.
 *
 * We exercise the protocol end-to-end with an in-process
 * `MessageChannel`: the test acts as the SW (sends a request via
 * `port2`), the bridge listener acts as the page (replies on
 * `port1`).
 */
import { afterEach, beforeEach, describe, expect, it } from "vitest";

import { saveVideoToOpfs, isOpfsSupported, OPFS_VIDEO_DIR } from "./offline-opfs.js";
import type { OpfsBridgeRequest, OpfsBridgeResponse } from "./opfs-bridge.js";
import type { VideoMetadata } from "../types/index.js";

const META: VideoMetadata = {
  id: "v",
  title: "Bridge Test",
  channel_id: null,
  channel_title: null,
  duration_seconds: 0,
  thumbnail_url: null,
  published_at: null,
};

async function clearOpfs(): Promise<void> {
  if (!isOpfsSupported()) return;
  try {
    const root = await navigator.storage.getDirectory();
    await root.removeEntry(OPFS_VIDEO_DIR, { recursive: true });
  } catch {
    // ignore
  }
}

/**
 * Local re-implementation of the bridge handler that takes a
 * `MessageEvent`-shaped payload + a port. Mirrors what
 * `registerOpfsBridge` does in the page; we replicate it here to
 * avoid relying on `navigator.serviceWorker` (which doesn't dispatch
 * events in test mode).
 */
async function handle(request: OpfsBridgeRequest, port: MessagePort): Promise<void> {
  const { hasOfflineVideo, getOfflineVideoRange } = await import("./offline-opfs.js");
  if (request.type === "opfs.exists") {
    const has = await hasOfflineVideo(request.videoId, request.quality);
    port.postMessage({
      ok: true,
      type: "opfs.exists.response",
      has,
    } satisfies OpfsBridgeResponse);
    return;
  }
  if (request.type === "opfs.read") {
    const start = request.range?.[0] ?? 0;
    const end = request.range?.[1] ?? Number.MAX_SAFE_INTEGER;
    const slice = await getOfflineVideoRange(request.videoId, request.quality, start, end);
    if (!slice) {
      port.postMessage({ ok: false } satisfies OpfsBridgeResponse);
      return;
    }
    const buffer = await slice.slice.arrayBuffer();
    const ranged = request.range !== undefined;
    const response: OpfsBridgeResponse = {
      ok: true,
      type: "opfs.read.response",
      body: buffer,
      contentType: slice.contentType,
      contentLength: buffer.byteLength,
    };
    if (ranged) {
      response.contentRange = {
        start,
        end: start + buffer.byteLength - 1,
        total: slice.total,
      };
    }
    port.postMessage(response, [buffer]);
  }
}

async function ask(req: OpfsBridgeRequest): Promise<OpfsBridgeResponse> {
  const channel = new MessageChannel();
  const reply = new Promise<OpfsBridgeResponse>((resolve) => {
    channel.port1.onmessage = (e) => {
      channel.port1.close();
      resolve(e.data as OpfsBridgeResponse);
    };
  });
  void handle(req, channel.port2);
  return reply;
}

describe("opfs-bridge protocol", () => {
  beforeEach(async () => {
    await clearOpfs();
  });
  afterEach(async () => {
    await clearOpfs();
  });

  it("responds to opfs.exists with `has: false` when the file is missing", async () => {
    const reply = await ask({
      type: "opfs.exists",
      videoId: "nope",
      quality: "720p",
    });
    expect(reply.ok).toBe(true);
    if (reply.ok && reply.type === "opfs.exists.response") {
      expect(reply.has).toBe(false);
    }
  });

  it("responds to opfs.exists with `has: true` after saving a video", async () => {
    const res = new Response(new Blob(["hello"]), { status: 200 });
    await saveVideoToOpfs("vid", "480p", res, META, "/api/x");

    const reply = await ask({
      type: "opfs.exists",
      videoId: "vid",
      quality: "480p",
    });
    expect(reply.ok).toBe(true);
    if (reply.ok && reply.type === "opfs.exists.response") {
      expect(reply.has).toBe(true);
    }
  });

  it("opfs.read returns the full body when no range is supplied", async () => {
    const res = new Response(new Blob(["abcdef"]), { status: 200 });
    await saveVideoToOpfs("vid", "480p", res, META, "/api/x");

    const reply = await ask({
      type: "opfs.read",
      videoId: "vid",
      quality: "480p",
    });
    expect(reply.ok).toBe(true);
    if (reply.ok && reply.type === "opfs.read.response") {
      const text = new TextDecoder().decode(reply.body);
      expect(text).toBe("abcdef");
      expect(reply.contentRange).toBeUndefined();
    }
  });

  it("opfs.read honours a byte range and reports Content-Range", async () => {
    const res = new Response(new Blob(["0123456789"]), { status: 200 });
    await saveVideoToOpfs("vid", "480p", res, META, "/api/x");

    const reply = await ask({
      type: "opfs.read",
      videoId: "vid",
      quality: "480p",
      range: [2, 5],
    });
    expect(reply.ok).toBe(true);
    if (reply.ok && reply.type === "opfs.read.response") {
      const text = new TextDecoder().decode(reply.body);
      expect(text).toBe("2345");
      expect(reply.contentRange).toEqual({ start: 2, end: 5, total: 10 });
    }
  });

  it("opfs.read replies with `ok: false` when the file is missing", async () => {
    const reply = await ask({
      type: "opfs.read",
      videoId: "missing",
      quality: "720p",
    });
    expect(reply.ok).toBe(false);
  });
});
