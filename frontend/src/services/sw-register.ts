/**
 * Tiny script that registers the HomeTube service worker.
 *
 * Loaded at the bottom of `templates/base.html`. Kept separate from the
 * theme/nav bundles so the SW can be registered before any other JS
 * runs.
 *
 * The SW file itself is emitted by `vite-plugin-pwa` to `/sw.js` at the
 * root of the dist directory.
 */

declare global {
  interface Window {
    /** Set to `true` to disable SW registration (used by dev mode). */
    __HOMETUBE_NO_SW?: boolean;
  }
}

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
