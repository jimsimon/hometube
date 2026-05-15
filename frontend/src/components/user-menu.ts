/**
 * <hometube-user-menu>
 *
 * Header-bar component rendered as an avatar/initials button that opens
 * a dropdown menu with the user's name, profile switch, and logout.
 * Uses <wa-dropdown> + <wa-dropdown-item> for proper positioning and a11y.
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
    }
    .trigger {
      display: inline-flex;
      align-items: center;
      justify-content: center;
      width: 2rem;
      height: 2rem;
      border-radius: 50%;
      border: 1px solid var(--wa-color-surface-border, #ccc);
      background: var(--wa-color-brand-quiet, rgba(37, 99, 235, 0.08));
      color: var(--wa-color-brand-on-quiet, #2563eb);
      font-weight: 700;
      font-size: 0.8rem;
      cursor: pointer;
      text-transform: uppercase;
      line-height: 1;
    }
    .trigger:hover {
      background: var(--wa-color-brand-fill, #2563eb);
      color: white;
    }
    .name-item {
      font-size: 0.85rem;
      color: var(--wa-color-text-quiet);
      padding: 0.5rem 0.75rem;
      border-bottom: 1px solid var(--wa-color-surface-border);
      margin-bottom: 0.25rem;
    }
  `;

  private get initials(): string {
    if (!this.displayName) return "?";
    const parts = this.displayName.trim().split(/\s+/);
    if (parts.length >= 2) return (parts[0][0] + parts[1][0]).toUpperCase();
    return this.displayName.substring(0, 2).toUpperCase();
  }

  private onLogout = async (): Promise<void> => {
    try {
      await api.post("/api/auth/logout");
    } finally {
      window.location.href = "/profiles";
    }
  };

  override render() {
    return html`
      <wa-dropdown>
        <button
          slot="trigger"
          class="trigger"
          aria-label="User menu for ${this.displayName || "account"}"
          title=${this.displayName || "Account"}
        >
          ${this.initials}
        </button>
          ${this.displayName ? html`<div class="name-item">${this.displayName}</div>` : nothing}
          ${!this.hideProfile
            ? html`<wa-dropdown-item
                value="profile"
                @click=${() => {
                  window.location.href = "/profiles";
                }}
              >
                Switch profile
              </wa-dropdown-item>`
            : nothing}
          ${!this.hideLogout
            ? html`<wa-dropdown-item value="logout" @click=${this.onLogout}> Log out </wa-dropdown-item>`
            : nothing}
      </wa-dropdown>
    `;
  }
}

declare global {
  interface HTMLElementTagNameMap {
    "hometube-user-menu": UserMenu;
  }
}
