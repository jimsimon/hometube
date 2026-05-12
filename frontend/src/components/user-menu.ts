/**
 * <hometube-user-menu>
 *
 * Header-bar component showing the signed-in user's name, a "Switch
 * profile" link, and a "Log out" button. Extracted from the old
 * <hometube-nav-bar> so these controls can live directly in the
 * <wa-page> header slot.
 */

import { LitElement, html, css, nothing } from "lit";
import { customElement, property } from "lit/decorators.js";

import { api } from "../services/api.js";

@customElement("hometube-user-menu")
export class UserMenu extends LitElement {
  /** Display name of the signed-in user. */
  @property({ type: String, attribute: "display-name" })
  displayName = "";

  /** Hide the "Switch profile" link. */
  @property({ type: Boolean, attribute: "hide-profile" })
  hideProfile = false;

  /** Hide the "Log out" button. */
  @property({ type: Boolean, attribute: "hide-logout" })
  hideLogout = false;

  static styles = css`
    :host {
      display: inline-flex;
      align-items: center;
      gap: 0.5rem;
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
    button.logout:hover {
      background: var(--wa-color-surface-raised);
    }
    a.profile {
      color: inherit;
      text-decoration: none;
      font-size: 0.85rem;
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
      ${this.displayName
        ? html`<span class="who">Signed in as ${this.displayName}</span>`
        : nothing}
      ${!this.hideProfile
        ? html`<a class="profile" href="/profiles">Switch profile</a>`
        : nothing}
      ${!this.hideLogout
        ? html`<button type="button" class="logout" @click=${this.onLogout}>Log out</button>`
        : nothing}
    `;
  }
}

declare global {
  interface HTMLElementTagNameMap {
    "hometube-user-menu": UserMenu;
  }
}
