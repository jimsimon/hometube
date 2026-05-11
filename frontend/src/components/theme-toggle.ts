/**
 * <hometube-theme-toggle>
 *
 * Lit web component that lets the user pick light, dark, or system
 * color scheme. The choice is persisted to localStorage by the
 * `services/theme` helpers.
 */

import { LitElement, html, css } from 'lit';
import { customElement, state } from 'lit/decorators.js';

import {
  applyTheme,
  getThemePreference,
  resolveTheme,
  setThemePreference,
  watchSystemTheme,
  type ThemePreference,
} from '../services/theme.js';

@customElement('hometube-theme-toggle')
export class ThemeToggle extends LitElement {
  static styles = css`
    :host {
      display: inline-flex;
      align-items: center;
      gap: 0.5rem;
    }

    label {
      font-size: 0.875rem;
      color: var(--wa-color-text-quiet);
    }

    select {
      padding: 0.25rem 0.5rem;
      border: 1px solid var(--wa-color-surface-border, #ccc);
      border-radius: 0.375rem;
      background: var(--wa-color-surface-default);
      color: var(--wa-color-text-normal);
      font: inherit;
    }
  `;

  @state()
  private pref: ThemePreference = 'system';

  private cleanupSystemListener?: () => void;

  override connectedCallback(): void {
    super.connectedCallback();
    this.pref = getThemePreference();
    this.cleanupSystemListener = watchSystemTheme((theme) => {
      if (this.pref === 'system') applyTheme(theme);
    });
  }

  override disconnectedCallback(): void {
    super.disconnectedCallback();
    this.cleanupSystemListener?.();
  }

  private onChange = (e: Event): void => {
    const value = (e.target as HTMLSelectElement).value as ThemePreference;
    this.pref = value;
    setThemePreference(value);
    applyTheme(resolveTheme(value));
  };

  override render() {
    return html`
      <label for="theme-select">Theme</label>
      <select
        id="theme-select"
        @change=${this.onChange}
        aria-label="Color theme"
      >
        <option value="system" ?selected=${this.pref === 'system'}>
          System
        </option>
        <option value="light" ?selected=${this.pref === 'light'}>
          Light
        </option>
        <option value="dark" ?selected=${this.pref === 'dark'}>
          Dark
        </option>
      </select>
    `;
  }
}

declare global {
  interface HTMLElementTagNameMap {
    'hometube-theme-toggle': ThemeToggle;
  }
}
