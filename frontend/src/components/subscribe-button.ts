/**
 * <hometube-subscribe-button channel-id="...">
 *
 * Toggles a child's subscription to a channel. On mount, fetches the
 * current subscription list and computes the initial state. Clicks
 * POST or DELETE against the `/api/subscriptions` endpoints.
 *
 * Emits a `hometube:subscription-changed` CustomEvent (bubbling) on
 * each successful toggle, with the new boolean state in `detail.subscribed`.
 */

import { LitElement, html, css } from "lit";
import { customElement, property, state } from "lit/decorators.js";

import { ApiError, api } from "../services/api.js";
import type { SubscriptionRow } from "../types/index.js";

@customElement("hometube-subscribe-button")
export class SubscribeButton extends LitElement {
  @property({ type: String, attribute: "channel-id" })
  channelId = "";

  @state() private subscribed = false;
  @state() private busy = false;
  @state() private error = "";

  static styles = css`
    :host {
      display: inline-block;
    }
    button {
      padding: 0.45rem 0.9rem;
      border-radius: 999px;
      border: 1px solid var(--wa-color-surface-border, #ccc);
      background: var(--wa-color-brand-fill, #2563eb);
      color: white;
      font: inherit;
      font-weight: 600;
      cursor: pointer;
    }
    button.subscribed {
      background: transparent;
      color: var(--wa-color-text-normal);
    }
    button[disabled] {
      opacity: 0.7;
      cursor: progress;
    }

    .error {
      color: var(--wa-color-danger-fill, #b91c1c);
      font-size: 0.85rem;
      margin-top: 0.25rem;
    }
  `;

  override connectedCallback(): void {
    super.connectedCallback();
    void this.refresh();
  }

  private async refresh(): Promise<void> {
    try {
      const subs = await api.get<SubscriptionRow[]>("/api/subscriptions");
      const match = subs.find((s) => s.channel_id === this.channelId);
      this.subscribed = !!match;
    } catch {
      // Silent — initial state stays "not subscribed".
    }
  }

  private async onToggle(): Promise<void> {
    if (this.busy || !this.channelId) return;
    this.busy = true;
    this.error = "";
    const wasSubscribed = this.subscribed;
    try {
      if (wasSubscribed) {
        await api.delete(`/api/subscriptions/${encodeURIComponent(this.channelId)}`);
        this.subscribed = false;
      } else {
        await api.post("/api/subscriptions", { channel_id: this.channelId });
        this.subscribed = true;
      }
      this.dispatchEvent(
        new CustomEvent("hometube:subscription-changed", {
          detail: { channelId: this.channelId, subscribed: this.subscribed },
          bubbles: true,
          composed: true,
        }),
      );
    } catch (err) {
      this.error = err instanceof ApiError ? String(err.body) : (err as Error).message;
    } finally {
      this.busy = false;
    }
  }

  override render() {
    const label = this.subscribed ? "Subscribed" : "Subscribe";
    return html`
      <button
        type="button"
        class=${this.subscribed ? "subscribed" : ""}
        ?disabled=${this.busy}
        aria-pressed=${this.subscribed}
        @click=${this.onToggle}
      >
        ${label}
      </button>
      ${this.error ? html`<div class="error" role="alert">${this.error}</div>` : null}
    `;
  }
}

declare global {
  interface HTMLElementTagNameMap {
    "hometube-subscribe-button": SubscribeButton;
  }
}
