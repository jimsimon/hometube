/**
 * <hometube-nav-drawer>
 *
 * Wraps Web Awesome's `<wa-drawer>` with the child-side navigation
 * link list that previously lived inline inside `<hometube-nav-child>`.
 * Splitting it out makes the link list reusable from any other place
 * that wants the same drawer (e.g. a future parent drawer for
 * mobile).
 *
 * Imperative API:
 *
 *   const drawer = document.querySelector('hometube-nav-drawer');
 *   drawer.show();
 *   drawer.hide();
 *
 * Slots:
 *   - default — replaces the built-in link list. Use this when the
 *     calling page wants an entirely custom drawer body.
 *
 * The default link list points at the Phase 9–11 child pages. Pages
 * that want a subset can pass their own `<ul>` markup via the
 * default slot.
 */

import { LitElement, html, css } from "lit";
import { customElement, property, query } from "lit/decorators.js";

@customElement("hometube-nav-drawer")
export class NavDrawer extends LitElement {
  /** ARIA label propagated to the underlying `<wa-drawer>`. */
  @property({ type: String })
  label = "Navigation";

  /** Drawer side. Mirrors `<wa-drawer placement>`. */
  @property({ type: String })
  placement: "start" | "end" | "top" | "bottom" = "start";

  @query("wa-drawer") private drawer!: HTMLElement & {
    open?: boolean;
    show?: () => void;
    hide?: () => void;
  };

  static styles = css`
    :host {
      display: contents;
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

  /** Open the drawer. */
  show(): void {
    this.drawer?.show?.();
  }

  /** Close the drawer. */
  hide(): void {
    this.drawer?.hide?.();
  }

  /** Toggle the drawer. */
  toggle(): void {
    if (!this.drawer) return;
    if (this.drawer.open) this.drawer.hide?.();
    else this.drawer.show?.();
  }

  override render() {
    return html`
      <wa-drawer label=${this.label} placement=${this.placement}>
        <slot>
          <ul class="drawer-list">
            <li><a href="/child/home">Home</a></li>
            <li><a href="/child/channels">Channels</a></li>
            <li><a href="/child/playlists">Playlists</a></li>
            <li><a href="/child/bookmarks">Bookmarks</a></li>
            <li><a href="/child/downloads">Downloads</a></li>
          </ul>
        </slot>
      </wa-drawer>
    `;
  }
}

declare global {
  interface HTMLElementTagNameMap {
    "hometube-nav-drawer": NavDrawer;
  }
}
