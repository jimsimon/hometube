/**
 * Service-worker ↔ page bridge for OPFS reads.
 *
 * Service workers cannot reach OPFS directly in current
 * specifications — only window/worker contexts can. To still serve
 * downloaded videos out of OPFS through the SW, every open page
 * registers itself as an OPFS proxy: the SW posts a message asking
 * "do you have this file?" / "give me these bytes", and one of the
 * pages answers via `MessageChannel`.
 *
 * The protocol is intentionally tiny:
 *
 *   {
 *     type: "opfs.exists",
 *     videoId: string,
 *     quality: string | null,
 *   } → { has: boolean }
 *
 *   {
 *     type: "opfs.read",
 *     videoId: string,
 *     quality: string,
 *     range?: [start, end],
 *   } → {
 *     ok: true,
 *     body: ArrayBuffer,
 *     contentType: string,
 *     contentLength: number,
 *     contentRange?: { start, end, total },
 *   } | { ok: false }
 *
 * Importing this file at top level (via `sw-register.ts`) installs
 * the listener. Calling it from non-browser contexts is a no-op.
 */
import { getOfflineVideoRange, hasOfflineVideo, isOpfsSupported } from "./offline-opfs.js";

export type OpfsBridgeRequest =
  | { type: "opfs.exists"; videoId: string; quality: string | null }
  | {
      type: "opfs.read";
      videoId: string;
      quality: string;
      range?: [number, number];
    };

export type OpfsBridgeResponse =
  | { ok: true; type: "opfs.exists.response"; has: boolean }
  | {
      ok: true;
      type: "opfs.read.response";
      body: ArrayBuffer;
      contentType: string;
      contentLength: number;
      contentRange?: { start: number; end: number; total: number };
    }
  | { ok: false; error?: string };

let installed = false;

/**
 * Register the OPFS-bridge `message` listener on this client.
 * Idempotent — calling more than once is a no-op.
 */
export function registerOpfsBridge(): void {
  if (installed) return;
  if (typeof window === "undefined") return;
  if (!isOpfsSupported()) return;

  installed = true;
  navigator.serviceWorker?.addEventListener("message", (event) => {
    void handleMessage(event);
  });
}

async function handleMessage(event: MessageEvent): Promise<void> {
  const data = event.data as OpfsBridgeRequest | undefined;
  if (!data || typeof data !== "object" || !("type" in data)) return;

  const port = event.ports[0];
  if (!port) return;

  try {
    if (data.type === "opfs.exists") {
      const has = await hasOfflineVideo(data.videoId, data.quality);
      port.postMessage({
        ok: true,
        type: "opfs.exists.response",
        has,
      } satisfies OpfsBridgeResponse);
      return;
    }

    if (data.type === "opfs.read") {
      const start = data.range?.[0] ?? 0;
      const end = data.range?.[1];
      // If a range was provided, slice it; otherwise read the full
      // file (`Number.MAX_SAFE_INTEGER` is clamped server-side).
      const slice = await getOfflineVideoRange(
        data.videoId,
        data.quality,
        start,
        end ?? Number.MAX_SAFE_INTEGER,
      );
      if (!slice) {
        port.postMessage({ ok: false } satisfies OpfsBridgeResponse);
        return;
      }
      const buffer = await slice.slice.arrayBuffer();
      const ranged = data.range !== undefined;
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
      return;
    }
  } catch (err) {
    port.postMessage({
      ok: false,
      error: (err as Error).message,
    } satisfies OpfsBridgeResponse);
  }
}
