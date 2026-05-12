/**
 * <hometube-nav-child>
 *
 * Top-bar navigation for the child UI. Composed from the shared
 * `<hometube-nav-bar>` shell plus a `<hometube-nav-drawer>` for the
 * left-side hamburger menu.
 */

import { LitElement, html, css } from "lit";
import { customElement, property, query } from "lit/decorators.js";

import "./search-bar.js";
import "./nav-bar.js";
import "./nav-drawer.js";
import type { NavDrawer } from "./nav-drawer.js";

@customElement("hometube-nav-child")
export class NavChild extends LitElement {
  @property({ type: String, attribute: "display-name" })
  displayName = "";

  @query("hometube-nav-drawer") private drawer!: NavDrawer;

  static styles = css`
    :host {
      display: block;
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
    button.drawer-toggle {
      padding: 0.5rem 0.75rem;
      border-radius: 0.375rem;
      border: 1px solid var(--wa-color-surface-border, #ccc);
      background: transparent;
      color: var(--wa-color-text-normal);
      font: inherit;
      cursor: pointer;
    }
  `;

  private toggleDrawer = (): void => {
    this.drawer?.toggle();
  };

  override render() {
    return html`
      <hometube-nav-bar nav-label="Main navigation" display-name=${this.displayName}>
        <button
          slot="brand"
          type="button"
          class="drawer-toggle"
          aria-label="Open navigation menu"
          @click=${this.toggleDrawer}
        >
          ☰
        </button>
        <a slot="brand" href="/child/home" class="brand">HomeTube</a>
        <div slot="primary" class="search">
          <hometube-search-bar></hometube-search-bar>
        </div>
        <hometube-theme-toggle slot="actions"></hometube-theme-toggle>
      </hometube-nav-bar>
      <hometube-nav-drawer></hometube-nav-drawer>
    `;
  }
}

declare global {
  interface HTMLElementTagNameMap {
    "hometube-nav-child": NavChild;
  }
}
