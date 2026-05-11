/**
 * <hometube-nav-parent>
 *
 * Top navigation bar shown on every parent dashboard page. Renders:
 *   - the HomeTube wordmark / link to /parent/home
 *   - section links (Content, Family, System)
 *   - a child-selector dropdown driven by /api/accounts?type=child
 *   - the global theme toggle
 *   - a logout button (POST /api/auth/logout)
 *
 * On every change of the child dropdown the component dispatches a
 * bubbling `child-changed` CustomEvent with `{ detail: { childId } }`,
 * which the page wiring in `templates/pages/parent/home.html` fans out
 * to every child-aware component on the page.
 */

import { LitElement, html, css } from 'lit';
import { customElement, property, state } from 'lit/decorators.js';

import { api } from '../services/api.js';
import type { AccountSummary } from '../types/index.js';

const SELECTED_CHILD_KEY = 'hometube-selected-child';

@customElement('hometube-nav-parent')
export class NavParent extends LitElement {
  @property({ type: String, attribute: 'display-name' })
  displayName = '';

  @state() private children: AccountSummary[] = [];
  @state() private selectedChildId: number | null = null;
  @state() private loading = false;
  @state() private error = '';

  static styles = css`
    :host {
      display: block;
    }
    nav {
      display: flex;
      flex-wrap: wrap;
      align-items: center;
      gap: 1rem;
      padding: 0.75rem 1rem;
      border-bottom: 1px solid var(--wa-color-surface-border);
      background: var(--wa-color-surface-default);
    }
    .brand {
      font-weight: 700;
      text-decoration: none;
      color: inherit;
    }
    .links {
      display: flex;
      gap: 1rem;
    }
    .links a {
      color: inherit;
      text-decoration: none;
    }
    .links a:hover {
      text-decoration: underline;
    }
    .spacer {
      flex: 1;
    }
    label.child-picker {
      display: inline-flex;
      align-items: center;
      gap: 0.5rem;
      font-size: 0.9rem;
    }
    select {
      padding: 0.25rem 0.5rem;
      border: 1px solid var(--wa-color-surface-border, #ccc);
      border-radius: 0.375rem;
      background: var(--wa-color-surface-default);
      color: var(--wa-color-text-normal);
      font: inherit;
    }
    button {
      padding: 0.25rem 0.75rem;
      border-radius: 0.375rem;
      border: 1px solid var(--wa-color-surface-border, #ccc);
      background: transparent;
      color: var(--wa-color-text-normal);
      font: inherit;
      cursor: pointer;
    }
    .who {
      color: var(--wa-color-text-quiet);
      font-size: 0.85rem;
    }
    .error {
      color: var(--wa-color-danger-fill, #b91c1c);
      font-size: 0.85rem;
    }
  `;

  override connectedCallback(): void {
    super.connectedCallback();
    void this.loadChildren();
  }

  private async loadChildren(): Promise<void> {
    this.loading = true;
    this.error = '';
    try {
      const list = await api.get<AccountSummary[]>('/api/accounts?type=child');
      this.children = list;
      const stored = Number(localStorage.getItem(SELECTED_CHILD_KEY));
      const fromStore = list.find((c) => c.id === stored);
      const initial = fromStore ?? list[0];
      if (initial) {
        this.selectedChildId = initial.id;
        this.dispatchChildChanged(initial.id);
      }
    } catch (err) {
      this.error = `Could not load children: ${(err as Error).message}`;
    } finally {
      this.loading = false;
    }
  }

  private dispatchChildChanged(childId: number): void {
    localStorage.setItem(SELECTED_CHILD_KEY, String(childId));
    this.dispatchEvent(
      new CustomEvent('child-changed', {
        detail: { childId },
        bubbles: true,
        composed: true,
      }),
    );
  }

  private onChildChange = (e: Event): void => {
    const id = Number((e.target as HTMLSelectElement).value);
    this.selectedChildId = id;
    this.dispatchChildChanged(id);
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
      <nav aria-label="Parent navigation">
        <a href="/parent/home" class="brand">HomeTube</a>

        <div class="links">
          <a href="/parent/home">Content</a>
          <a href="/parent/family">Family</a>
          <a href="/parent/system">System</a>
        </div>

        <div class="spacer"></div>

        ${this.loading
          ? html`<span class="who">Loading…</span>`
          : this.children.length === 0
            ? html`<span class="who">No children yet</span>`
            : html`
                <label class="child-picker" for="child-select">
                  Child
                  <select
                    id="child-select"
                    aria-label="Active child"
                    .value=${String(this.selectedChildId ?? '')}
                    @change=${this.onChildChange}
                  >
                    ${this.children.map(
                      (c) =>
                        html`<option
                          value=${c.id}
                          ?selected=${c.id === this.selectedChildId}
                        >
                          ${c.display_name}
                        </option>`,
                    )}
                  </select>
                </label>
              `}

        <hometube-theme-toggle></hometube-theme-toggle>

        <span class="who" aria-hidden="true">
          ${this.displayName ? `Signed in as ${this.displayName}` : ''}
        </span>
        <button type="button" @click=${this.onLogout}>Log out</button>
        ${this.error
          ? html`<span class="error" role="alert">${this.error}</span>`
          : null}
      </nav>
    `;
  }
}

declare global {
  interface HTMLElementTagNameMap {
    'hometube-nav-parent': NavParent;
  }
}
