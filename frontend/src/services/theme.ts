/**
 * Theme preference helpers.
 *
 * The user's preference (`light`, `dark`, or `system`) is persisted in
 * localStorage. The actual color scheme applied to <html> is one of
 * `wa-theme-light` or `wa-theme-dark` — when the preference is `system`,
 * we resolve it via `prefers-color-scheme`.
 *
 * Per-account scoping (T-5):
 *
 *   The original implementation used a single `hometube-theme` key.
 *   That meant switching profiles via /profiles still showed whichever
 *   theme the previous account picked. We now namespace the key by
 *   account ID once it's known: `hometube-theme:<accountId>`.
 *
 *   Boot order:
 *     1. base.html runs an inline script that reads the legacy
 *        `hometube-theme` key and applies the resulting wa-theme-*
 *        class as early as possible (no FOUC for anonymous users).
 *     2. After the page hydrates, `<hometube-theme-toggle>` calls
 *        `setAccountScope(accountId)` once /api/auth/me resolves;
 *        if a per-account preference exists it overrides the
 *        legacy choice.
 *     3. `setThemePreference` writes to the scoped key whenever an
 *        account is active.
 *
 *   The legacy `hometube-theme` key is still read as a fallback so
 *   pre-T-5 installs migrate transparently — the next
 *   `setThemePreference` call after a profile switch promotes the
 *   value into the scoped key.
 */

export type ThemePreference = "light" | "dark" | "system";
export type ResolvedTheme = "light" | "dark";

const LEGACY_KEY = "hometube-theme";

/** Currently-active account scope, or `null` for anonymous boots. */
let currentScope: number | string | null = null;

/** Build the localStorage key for an optional account scope. */
function storageKey(scope: number | string | null = currentScope): string {
  return scope == null ? LEGACY_KEY : `${LEGACY_KEY}:${scope}`;
}

function readKey(key: string): ThemePreference | null {
  try {
    const v = localStorage.getItem(key);
    if (v === "light" || v === "dark" || v === "system") return v;
  } catch {
    // localStorage may be blocked (e.g. Safari private mode).
  }
  return null;
}

/**
 * Bind the theme service to a specific account ID. After this call,
 * subsequent `getThemePreference` / `setThemePreference` operate on
 * `hometube-theme:<accountId>`. If a per-account preference exists,
 * `applyTheme` is invoked immediately so the page reflects the
 * change. Returns the preference that ended up applied.
 */
export function setAccountScope(accountId: number | string | null): ThemePreference {
  currentScope = accountId;
  const pref = getThemePreference();
  applyTheme(resolveTheme(pref));
  return pref;
}

/** Clear the account scope (used on logout). */
export function clearAccountScope(): void {
  currentScope = null;
}

export function getThemePreference(): ThemePreference {
  // Prefer the scoped key when an account is known; fall back to the
  // legacy key so pre-T-5 installs and anonymous sessions keep
  // behaving as before.
  if (currentScope != null) {
    const scoped = readKey(storageKey());
    if (scoped) return scoped;
  }
  return readKey(LEGACY_KEY) ?? "system";
}

export function setThemePreference(pref: ThemePreference): void {
  try {
    localStorage.setItem(storageKey(), pref);
    // Mirror to the legacy key when no scope is active so the
    // pre-paint bootstrap in base.html still gets the right value
    // on next reload for anonymous flows.
    if (currentScope == null) {
      localStorage.setItem(LEGACY_KEY, pref);
    }
  } catch {
    // Best-effort; ignore failures.
  }
  applyTheme(resolveTheme(pref));
}

export function resolveTheme(pref: ThemePreference): ResolvedTheme {
  if (pref !== "system") return pref;
  return matchMedia("(prefers-color-scheme: dark)").matches ? "dark" : "light";
}

export function applyTheme(theme: ResolvedTheme): void {
  const html = document.documentElement;
  html.classList.remove("wa-theme-light", "wa-theme-dark");
  html.classList.add(`wa-theme-${theme}`);
}

/**
 * Listen for OS-level color-scheme changes when the preference is
 * `system`. Returns a cleanup function.
 */
export function watchSystemTheme(onChange: (theme: ResolvedTheme) => void): () => void {
  const mql = matchMedia("(prefers-color-scheme: dark)");
  const handler = (e: MediaQueryListEvent) => {
    onChange(e.matches ? "dark" : "light");
  };
  mql.addEventListener("change", handler);
  return () => mql.removeEventListener("change", handler);
}
