/**
 * Unit tests for the theme service.
 */

import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';

import {
  applyTheme,
  getThemePreference,
  resolveTheme,
  setThemePreference,
} from './theme.js';

describe('theme service', () => {
  beforeEach(() => {
    localStorage.clear();
    document.documentElement.className = '';
  });

  afterEach(() => {
    vi.restoreAllMocks();
  });

  it('returns "system" when nothing is stored', () => {
    expect(getThemePreference()).toBe('system');
  });

  it('round-trips a stored preference', () => {
    setThemePreference('dark');
    expect(getThemePreference()).toBe('dark');
  });

  it('falls back to "system" for unknown values', () => {
    localStorage.setItem('hometube-theme', 'mauve');
    expect(getThemePreference()).toBe('system');
  });

  it('resolves light/dark passthrough', () => {
    expect(resolveTheme('light')).toBe('light');
    expect(resolveTheme('dark')).toBe('dark');
  });

  it('resolves "system" via matchMedia', () => {
    vi.spyOn(window, 'matchMedia').mockReturnValue({
      matches: true,
      media: '(prefers-color-scheme: dark)',
      addEventListener: vi.fn(),
      removeEventListener: vi.fn(),
      addListener: vi.fn(),
      removeListener: vi.fn(),
      onchange: null,
      dispatchEvent: vi.fn(),
    } as unknown as MediaQueryList);
    expect(resolveTheme('system')).toBe('dark');
  });

  it('applyTheme swaps the wa-theme-* class', () => {
    applyTheme('light');
    expect(document.documentElement.classList.contains('wa-theme-light')).toBe(true);
    applyTheme('dark');
    expect(document.documentElement.classList.contains('wa-theme-light')).toBe(false);
    expect(document.documentElement.classList.contains('wa-theme-dark')).toBe(true);
  });
});
