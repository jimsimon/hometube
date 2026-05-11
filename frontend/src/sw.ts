/**
 * HomeTube service worker.
 *
 * Phase 1 stub. Later phases will:
 *   - precache the app shell (Workbox `injectManifest` mode)
 *   - intercept /api/proxy/video and /api/downloads/:videoId/stream
 *     requests and serve from OPFS when the video is downloaded
 *   - handle offline detection and graceful degradation
 *
 * For now it installs cleanly and immediately claims clients so it
 * doesn't interfere with development.
 */

/// <reference lib="webworker" />
declare const self: ServiceWorkerGlobalScope;

self.addEventListener('install', () => {
  // Activate immediately.
  void self.skipWaiting();
});

self.addEventListener('activate', (event: ExtendableEvent) => {
  event.waitUntil(self.clients.claim());
});

// No fetch handler yet — requests pass through to the network.
export {};
