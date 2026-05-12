/**
 * Service-worker bootstrap.
 *
 * Loaded at the bottom of `templates/base.html`. Kept separate from
 * the theme/nav bundles so the SW can be registered before any other
 * JS runs.
 *
 * In addition to registering the SW, this module:
 *   - Installs the OPFS bridge so the SW can serve downloaded videos
 *     via `postMessage` (T-11 of the follow-up plan).
 *   - Runs a one-shot migration that copies any leftover Cache-API
 *     downloads into OPFS and clears the old cache.
 */

import { migrateCacheStorageToOpfs } from "./offline.js";
import { registerOpfsBridge } from "./opfs-bridge.js";

declare global {
  interface Window {
    /** Set to `true` to disable SW registration (used by dev mode). */
    __HOMETUBE_NO_SW?: boolean;
  }
}

// Always install the OPFS bridge — even if the SW isn't registered
// (dev mode or http://) the bridge is harmless and ready in case a
// future SW activates.
registerOpfsBridge();

// Best-effort migration. Runs at most once per profile thanks to the
// internal `localStorage` flag.
void migrateCacheStorageToOpfs();

if (
  "serviceWorker" in navigator &&
  !window.__HOMETUBE_NO_SW &&
  // Don't register on `http://` (except localhost) — most browsers
  // refuse anyway, and it just spams the console.
  (location.protocol === "https:" || location.hostname === "localhost")
) {
  window.addEventListener("load", () => {
    navigator.serviceWorker.register("/sw.js", { scope: "/" }).catch((err: unknown) => {
      // Silent failure is fine; the app works without the SW.
      console.warn("HomeTube: service worker registration failed", err);
    });
  });
}

export {};
