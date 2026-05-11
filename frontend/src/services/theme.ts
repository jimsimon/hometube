/**
 * Theme preference helpers.
 *
 * The user's preference (`light`, `dark`, or `system`) is persisted in
 * localStorage. The actual color scheme applied to <html> is one of
 * `wa-theme-light` or `wa-theme-dark` — when the preference is `system`,
 * we resolve it via `prefers-color-scheme`.
 */

export type ThemePreference = 'light' | 'dark' | 'system';
export type ResolvedTheme = 'light' | 'dark';

const STORAGE_KEY = 'hometube-theme';

export function getThemePreference(): ThemePreference {
  try {
    const v = localStorage.getItem(STORAGE_KEY);
    if (v === 'light' || v === 'dark' || v === 'system') return v;
  } catch {
    // localStorage may be blocked (e.g. Safari private mode).
  }
  return 'system';
}

export function setThemePreference(pref: ThemePreference): void {
  try {
    localStorage.setItem(STORAGE_KEY, pref);
  } catch {
    // Best-effort; ignore failures.
  }
  applyTheme(resolveTheme(pref));
}

export function resolveTheme(pref: ThemePreference): ResolvedTheme {
  if (pref !== 'system') return pref;
  return matchMedia('(prefers-color-scheme: dark)').matches ? 'dark' : 'light';
}

export function applyTheme(theme: ResolvedTheme): void {
  const html = document.documentElement;
  html.classList.remove('wa-theme-light', 'wa-theme-dark');
  html.classList.add(`wa-theme-${theme}`);
}

/**
 * Listen for OS-level color-scheme changes when the preference is
 * `system`. Returns a cleanup function.
 */
export function watchSystemTheme(onChange: (theme: ResolvedTheme) => void): () => void {
  const mql = matchMedia('(prefers-color-scheme: dark)');
  const handler = (e: MediaQueryListEvent) => {
    onChange(e.matches ? 'dark' : 'light');
  };
  mql.addEventListener('change', handler);
  return () => mql.removeEventListener('change', handler);
}
