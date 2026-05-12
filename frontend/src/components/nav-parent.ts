/**
 * <hometube-nav-parent>
 *
 * Top navigation for the parent dashboard. Composed of:
 *
 *   - <hometube-nav-bar>           — the responsive shell + logout
 *   - <hometube-account-selector>  — child-picker dropdown
 *   - <hometube-notification-bell> — alerts pill
 *   - <hometube-theme-toggle>      — dark/light switch
 *
 * On every change of the child dropdown the component dispatches a
 * bubbling `child-changed` CustomEvent with `{ detail: { childId } }`,
 * which the page wiring in `templates/pages/parent/home.html` fans
 * out to every child-aware component on the page.
 */

import { LitElement, html, css } from "lit";
import { customElement, property } from "lit/decorators.js";

import "./notification-bell.js";
import "./nav-bar.js";
import "./account-selector.js";

@customElement("hometube-nav-parent")
export class NavParent extends LitElement {
  @property({ type: String, attribute: "display-name" })
  displayName = "";

  static styles = css`
    :host {
      display: block;
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
    .brand {
      font-weight: 700;
      text-decoration: none;
      color: inherit;
    }
  `;

  private onAccountChanged = (
    e: CustomEvent<{ accountId?: number; accountIds?: number[] }>,
  ): void => {
    const id = e.detail.accountId;
    if (id != null) {
      this.dispatchEvent(
        new CustomEvent("child-changed", {
          detail: { childId: id },
          bubbles: true,
          composed: true,
        }),
      );
    }
  };

  override render() {
    return html`
      <hometube-nav-bar nav-label="Parent navigation" display-name=${this.displayName}>
        <a slot="brand" href="/parent/home" class="brand">HomeTube</a>
        <div slot="primary" class="links">
          <a href="/parent/home">Content</a>
          <a href="/parent/playlists">Playlists</a>
          <a href="/parent/activity">Activity</a>
          <a href="/parent/family">Family</a>
          <a href="/parent/system">System</a>
        </div>
        <hometube-account-selector
          slot="actions"
          account-type="child"
          empty-message="No children yet"
          @account-changed=${this.onAccountChanged}
        ></hometube-account-selector>
        <hometube-notification-bell slot="actions"></hometube-notification-bell>
        <hometube-theme-toggle slot="actions"></hometube-theme-toggle>
      </hometube-nav-bar>
    `;
  }
}

declare global {
  interface HTMLElementTagNameMap {
    "hometube-nav-parent": NavParent;
  }
}
