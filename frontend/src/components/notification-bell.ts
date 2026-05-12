/**
 * <hometube-notification-bell>
 *
 * Bell icon + unread-count badge in the parent nav bar. Clicking the
 * bell opens a panel with the latest notifications and per-row
 * "Mark read" / "Dismiss" actions. The unread count is polled every
 * 60 seconds.
 *
 * The panel is rendered inline (a positioned `<div>`) rather than a
 * `<wa-dropdown>` so we don't need to ship the Web Awesome dropdown
 * component for this nav element. The button drives `aria-expanded`
 * + `aria-controls` for keyboard / SR users.
 */

import { LitElement, html, css, nothing } from "lit";
import { customElement, state } from "lit/decorators.js";

import { ApiError, api } from "../services/api.js";
import type { NotificationRow } from "../types/index.js";

import "./loading-spinner.js";
import "./error-banner.js";

const POLL_INTERVAL_MS = 60_000;

@customElement("hometube-notification-bell")
export class NotificationBell extends LitElement {
  @state() private unread = 0;
  @state() private items: NotificationRow[] = [];
  @state() private open = false;
  @state() private loading = false;
  @state() private error = "";

  private pollHandle: number | null = null;

  static styles = css`
    :host {
      position: relative;
      display: inline-block;
    }
    button.bell {
      position: relative;
      padding: 0.25rem 0.5rem;
      border: 1px solid var(--wa-color-surface-border, #ccc);
      border-radius: 0.375rem;
      background: transparent;
      color: var(--wa-color-text-normal);
      font: inherit;
      cursor: pointer;
    }
    button.bell:focus-visible {
      outline: 2px solid var(--wa-color-brand-fill, #2563eb);
      outline-offset: 2px;
    }
    .badge {
      position: absolute;
      top: -0.4rem;
      right: -0.4rem;
      min-width: 1.1rem;
      height: 1.1rem;
      padding: 0 0.25rem;
      border-radius: 999px;
      background: var(--wa-color-danger-fill, #b91c1c);
      color: white;
      font-size: 0.7rem;
      line-height: 1.1rem;
      text-align: center;
      box-sizing: border-box;
    }
    .panel {
      position: absolute;
      top: calc(100% + 0.5rem);
      right: 0;
      min-width: 22rem;
      max-width: min(28rem, 90vw);
      max-height: 70vh;
      overflow-y: auto;
      padding: 0.5rem;
      border: 1px solid var(--wa-color-surface-border, #ccc);
      border-radius: 0.5rem;
      background: var(--wa-color-surface-default);
      color: var(--wa-color-text-normal);
      box-shadow: 0 0.5rem 1.5rem rgba(0, 0, 0, 0.15);
      z-index: 1000;
    }
    .empty {
      padding: 1rem;
      color: var(--wa-color-text-quiet);
      font-style: italic;
      text-align: center;
    }
    ul {
      list-style: none;
      margin: 0;
      padding: 0;
      display: grid;
      gap: 0.25rem;
    }
    li {
      padding: 0.5rem;
      border-radius: 0.375rem;
      border: 1px solid transparent;
      display: grid;
      grid-template-columns: 1fr auto;
      gap: 0.25rem 0.75rem;
    }
    li.unread {
      background: var(--wa-color-brand-quiet, rgba(37, 99, 235, 0.08));
      border-color: var(--wa-color-brand-fill, #2563eb);
    }
    .title {
      font-weight: 600;
      font-size: 0.95rem;
    }
    .message {
      grid-column: 1 / -1;
      font-size: 0.9rem;
      color: var(--wa-color-text-quiet);
    }
    .timestamp {
      font-size: 0.75rem;
      color: var(--wa-color-text-quiet);
    }
    .actions {
      display: flex;
      gap: 0.25rem;
      grid-column: 1 / -1;
      justify-content: flex-end;
      margin-top: 0.25rem;
    }
    .actions button {
      padding: 0.2rem 0.5rem;
      font-size: 0.85rem;
      border-radius: 0.25rem;
      border: 1px solid var(--wa-color-surface-border, #ccc);
      background: transparent;
      color: inherit;
      cursor: pointer;
    }
    .footer {
      display: flex;
      justify-content: flex-end;
      padding-top: 0.5rem;
      border-top: 1px solid var(--wa-color-surface-border, #ccc);
      margin-top: 0.5rem;
    }
    .footer button {
      padding: 0.35rem 0.75rem;
      border-radius: 0.375rem;
      border: 1px solid var(--wa-color-surface-border, #ccc);
      background: var(--wa-color-brand-fill, #2563eb);
      color: white;
      cursor: pointer;
    }
    .error {
      padding: 0.5rem;
      color: var(--wa-color-danger-fill, #b91c1c);
      font-size: 0.9rem;
    }
  `;

  override connectedCallback(): void {
    super.connectedCallback();
    void this.refreshUnread();
    this.pollHandle = window.setInterval(() => void this.refreshUnread(), POLL_INTERVAL_MS);
    document.addEventListener("click", this.onDocumentClick);
  }

  override disconnectedCallback(): void {
    super.disconnectedCallback();
    if (this.pollHandle != null) {
      window.clearInterval(this.pollHandle);
      this.pollHandle = null;
    }
    document.removeEventListener("click", this.onDocumentClick);
  }

  private onDocumentClick = (e: Event): void => {
    if (!this.open) return;
    const target = e.target as Node;
    if (!this.contains(target)) {
      this.open = false;
    }
  };

  private async refreshUnread(): Promise<void> {
    try {
      const res = await api.get<{ unread: number }>("/api/notifications/unread-count");
      this.unread = res.unread;
    } catch {
      // Silent: a transient API failure shouldn't blow up the nav bar.
    }
  }

  private toggleOpen = async (e: Event): Promise<void> => {
    e.stopPropagation();
    this.open = !this.open;
    if (this.open) {
      await this.loadItems();
    }
  };

  private async loadItems(): Promise<void> {
    this.loading = true;
    this.error = "";
    try {
      this.items = await api.get<NotificationRow[]>("/api/notifications?limit=20");
    } catch (err) {
      this.error = err instanceof ApiError ? String(err.body) : (err as Error).message;
    } finally {
      this.loading = false;
    }
  }

  private async markRead(id: number): Promise<void> {
    try {
      await api.put(`/api/notifications/${id}/read`);
      this.items = this.items.map((n) => (n.id === id ? { ...n, is_read: 1 } : n));
      void this.refreshUnread();
    } catch (err) {
      this.error = (err as Error).message;
    }
  }

  private async dismiss(id: number): Promise<void> {
    try {
      await api.delete(`/api/notifications/${id}`);
      this.items = this.items.filter((n) => n.id !== id);
      void this.refreshUnread();
    } catch (err) {
      this.error = (err as Error).message;
    }
  }

  private async markAllRead(): Promise<void> {
    try {
      await api.put("/api/notifications/read-all");
      this.items = this.items.map((n) => ({ ...n, is_read: 1 }));
      this.unread = 0;
    } catch (err) {
      this.error = (err as Error).message;
    }
  }

  private formatTime(unix: number): string {
    const d = new Date(unix * 1000);
    return d.toLocaleString();
  }

  override render() {
    const badge =
      this.unread > 0
        ? html`<span class="badge" aria-hidden="true"
            >${this.unread > 99 ? "99+" : this.unread}</span
          >`
        : nothing;
    return html`
      <button
        type="button"
        class="bell"
        aria-label=${`Notifications${this.unread > 0 ? ` (${this.unread} unread)` : ""}`}
        aria-haspopup="true"
        aria-expanded=${this.open ? "true" : "false"}
        aria-controls="notification-panel"
        @click=${this.toggleOpen}
      >
        🔔 ${badge}
      </button>
      ${this.open
        ? html`
            <div id="notification-panel" class="panel" role="dialog" aria-label="Notifications">
              ${this.error
                ? html`<hometube-error-banner .message=${this.error}></hometube-error-banner>`
                : nothing}
              ${this.loading
                ? html`<hometube-loading-spinner
                    label="Loading notifications…"
                  ></hometube-loading-spinner>`
                : this.items.length === 0
                  ? html`<p class="empty">No notifications.</p>`
                  : html`
                      <ul>
                        ${this.items.map(
                          (n) => html`
                            <li class=${n.is_read === 0 ? "unread" : ""}>
                              <span class="title">${n.title}</span>
                              <span class="timestamp"> ${this.formatTime(n.created_at)} </span>
                              <span class="message">${n.message}</span>
                              <span class="actions">
                                ${n.is_read === 0
                                  ? html`<button
                                      type="button"
                                      @click=${() => void this.markRead(n.id)}
                                    >
                                      Mark read
                                    </button>`
                                  : nothing}
                                <button
                                  type="button"
                                  aria-label="Dismiss notification"
                                  @click=${() => void this.dismiss(n.id)}
                                >
                                  Dismiss
                                </button>
                              </span>
                            </li>
                          `,
                        )}
                      </ul>
                      ${this.unread > 0
                        ? html`<div class="footer">
                            <button type="button" @click=${() => void this.markAllRead()}>
                              Mark all read
                            </button>
                          </div>`
                        : nothing}
                    `}
            </div>
          `
        : nothing}
    `;
  }
}

declare global {
  interface HTMLElementTagNameMap {
    "hometube-notification-bell": NotificationBell;
  }
}
