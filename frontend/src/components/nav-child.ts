/**
 * <hometube-nav-child>
 *
 * Top-bar navigation for the child UI. Includes:
 *   - logo / link to /child/home
 *   - search input (Enter submits to /child/search?q=...)
 *   - drawer toggle button
 *   - logout button
 *
 * The drawer (Web Awesome <wa-drawer>) is rendered inline so any page
 * including this nav gets it for free. Bookmarks/links not yet
 * implemented are listed but link to placeholders.
 */

import { LitElement, html, css } from 'lit';
import { customElement, property, query } from 'lit/decorators.js';

import { api } from '../services/api.js';

import './search-bar.js';

@customElement('hometube-nav-child')
export class NavChild extends LitElement {
  @property({ type: String, attribute: 'display-name' })
  displayName = '';

  @query('wa-drawer') private drawer!: HTMLElement & {
    open?: boolean;
    show?: () => void;
    hide?: () => void;
  };

  static styles = css`
    :host {
      display: block;
    }
    nav {
      display: flex;
      align-items: center;
      gap: 0.75rem;
      padding: 0.75rem 1rem;
      border-bottom: 1px solid var(--wa-color-surface-border);
      background: var(--wa-color-surface-default);
    }
    .brand {
      font-weight: 700;
      text-decoration: none;
      color: inherit;
    }
    .search {
      flex: 1;
      max-width: 32rem;
      display: flex;
      gap: 0.5rem;
    }
    input[type='search'] {
      flex: 1;
      padding: 0.5rem;
      border: 1px solid var(--wa-color-surface-border, #ccc);
      border-radius: 0.375rem;
      background: var(--wa-color-surface-default);
      color: var(--wa-color-text-normal);
      font: inherit;
    }
    button {
      padding: 0.5rem 0.75rem;
      border-radius: 0.375rem;
      border: 1px solid var(--wa-color-surface-border, #ccc);
      background: transparent;
      color: var(--wa-color-text-normal);
      font: inherit;
      cursor: pointer;
    }
    .drawer-list {
      list-style: none;
      padding: 0;
      margin: 0;
      display: grid;
      gap: 0.5rem;
    }
    .drawer-list a {
      display: block;
      padding: 0.5rem 0.75rem;
      border-radius: 0.25rem;
      color: inherit;
      text-decoration: none;
    }
    .drawer-list a:hover,
    .drawer-list a:focus-visible {
      background: var(--wa-color-surface-raised);
      outline: none;
    }
  `;

  private toggleDrawer = (): void => {
    if (!this.drawer) return;
    if (this.drawer.open) {
      this.drawer.hide?.();
    } else {
      this.drawer.show?.();
    }
  };

  private onLogout = async (): Promise<void> => {
    try {
      await api.post('/api/auth/logout');
    } finally {
      window.location.href = '/';
    }
  };

  override render() {
    return html`
      <nav aria-label="Main navigation">
        <button
          type="button"
          aria-label="Open navigation menu"
          @click=${this.toggleDrawer}
        >
          ☰
        </button>
        <a href="/child/home" class="brand">HomeTube</a>
        <div class="search">
          <hometube-search-bar></hometube-search-bar>
        </div>
        <hometube-theme-toggle></hometube-theme-toggle>
        ${this.displayName
          ? html`<span aria-hidden="true">${this.displayName}</span>`
          : null}
        <button type="button" @click=${this.onLogout}>Log out</button>
      </nav>

      <wa-drawer label="Navigation" placement="start">
        <ul class="drawer-list">
          <li><a href="/child/home">Home</a></li>
          <li><a href="/child/channels">Channels</a></li>
          <li><a href="/child/playlists">Playlists</a></li>
          <li><a href="/child/bookmarks">Bookmarks</a></li>
        </ul>
      </wa-drawer>
    `;
  }
}

declare global {
  interface HTMLElementTagNameMap {
    'hometube-nav-child': NavChild;
  }
}
