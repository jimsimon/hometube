/**
 * <hometube-nav-bar>
 *
 * Slot-based top-bar shell shared by the parent and child navs. Owns
 * the responsive flex layout, the surrounding chrome (border, padding,
 * background), and the logout button — three concerns the parent and
 * child navs were duplicating before T-9.
 *
 * Slots:
 *
 *   - `brand`    — wordmark / left-most link (e.g. "HomeTube")
 *   - `primary`  — primary navigation links + drawer toggle. Stretches
 *                  to fill available space.
 *   - `actions`  — right-side controls (notification bell, theme
 *                  toggle, child selector, etc.). Always present
 *                  before the built-in profile/logout buttons.
 *
 * The component automatically renders a "Switch profile" link to
 * `/profiles` and a "Log out" button that POSTs `/api/auth/logout`
 * and redirects to `/profiles`. Toggle them via the `hide-profile`
 * and `hide-logout` boolean attributes.
 */

import { LitElement, html, css, nothing } from "lit";
import { customElement, property } from "lit/decorators.js";

import { api } from "../services/api.js";

@customElement("hometube-nav-bar")
export class NavBar extends LitElement {
  /** ARIA label exposed on the underlying `<nav>` element. */
  @property({ type: String, attribute: "nav-label" })
  navLabel = "Main navigation";

  /** Optional display name; rendered after the actions slot when set. */
  @property({ type: String, attribute: "display-name" })
  displayName = "";

  /** Hide the built-in "Switch profile" link. */
  @property({ type: Boolean, attribute: "hide-profile" })
  hideProfile = false;

  /** Hide the built-in "Log out" button. */
  @property({ type: Boolean, attribute: "hide-logout" })
  hideLogout = false;

  static styles = css`
    :host {
      display: block;
    }
    nav {
      display: flex;
      flex-wrap: wrap;
      align-items: center;
      gap: 0.75rem;
      padding: 0.75rem 1rem;
      border-bottom: 1px solid var(--wa-color-surface-border);
      background: var(--wa-color-surface-default);
    }
    .primary {
      display: flex;
      align-items: center;
      gap: 1rem;
      flex: 1;
      min-width: 0;
    }
    .actions {
      display: flex;
      align-items: center;
      gap: 0.75rem;
      flex-wrap: wrap;
    }
    .who {
      color: var(--wa-color-text-quiet);
      font-size: 0.85rem;
    }
    button.logout {
      padding: 0.25rem 0.75rem;
      border-radius: 0.375rem;
      border: 1px solid var(--wa-color-surface-border, #ccc);
      background: transparent;
      color: var(--wa-color-text-normal);
      font: inherit;
      cursor: pointer;
    }
    a.profile {
      color: inherit;
      text-decoration: none;
    }
    a.profile:hover {
      text-decoration: underline;
    }
  `;

  private onLogout = async (): Promise<void> => {
    try {
      await api.post("/api/auth/logout");
    } finally {
      window.location.href = "/profiles";
    }
  };

  override render() {
    return html`
      <nav aria-label=${this.navLabel}>
        <slot name="brand"></slot>
        <div class="primary">
          <slot name="primary"></slot>
        </div>
        <div class="actions">
          <slot name="actions"></slot>
          ${this.displayName
            ? html`<span class="who" aria-hidden="true"> Signed in as ${this.displayName} </span>`
            : nothing}
          ${!this.hideProfile
            ? html`<a class="profile" href="/profiles">Switch profile</a>`
            : nothing}
          ${!this.hideLogout
            ? html`<button type="button" class="logout" @click=${this.onLogout}>Log out</button>`
            : nothing}
        </div>
      </nav>
    `;
  }
}

declare global {
  interface HTMLElementTagNameMap {
    "hometube-nav-bar": NavBar;
  }
}
