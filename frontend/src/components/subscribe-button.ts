/**
 * <hometube-subscribe-button channel-id="...">
 *
 * Toggles a child's subscription to a channel. On mount, fetches the
 * current subscription list and computes the initial state. Clicks
 * POST or DELETE against the `/api/subscriptions` endpoints; while a
 * push is in flight the button shows a "pending" state by polling
 * the list endpoint.
 *
 * Emits a `hometube:subscription-changed` CustomEvent (bubbling) on
 * each successful toggle, with the new boolean state in `detail.subscribed`.
 */

import { LitElement, html, css } from 'lit';
import { customElement, property, state } from 'lit/decorators.js';

import { ApiError, api } from '../services/api.js';
import type { SubscriptionRow, SyncStatus } from '../types/index.js';

const POLL_INTERVAL_MS = 2_000;
const POLL_MAX_ATTEMPTS = 10;

@customElement('hometube-subscribe-button')
export class SubscribeButton extends LitElement {
  @property({ type: String, attribute: 'channel-id' })
  channelId = '';

  @state() private subscribed = false;
  @state() private syncStatus: SyncStatus | null = null;
  @state() private busy = false;
  @state() private error = '';

  private pollTimer: number | null = null;

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
    .pending-indicator {
      display: inline-block;
      width: 0.6rem;
      height: 0.6rem;
      border-radius: 50%;
      margin-right: 0.3rem;
      background: var(--wa-color-warning-fill, #d97706);
      animation: pulse 1s ease-in-out infinite;
    }
    @keyframes pulse {
      0%, 100% { opacity: 0.4; }
      50% { opacity: 1; }
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

  override disconnectedCallback(): void {
    super.disconnectedCallback();
    this.stopPolling();
  }

  private async refresh(): Promise<void> {
    try {
      const subs = await api.get<SubscriptionRow[]>('/api/subscriptions');
      const match = subs.find((s) => s.channel_id === this.channelId);
      this.subscribed = !!match;
      this.syncStatus = match ? match.sync_status : null;
    } catch {
      // Silent — initial state stays "not subscribed".
    }
  }

  private async onToggle(): Promise<void> {
    if (this.busy || !this.channelId) return;
    this.busy = true;
    this.error = '';
    const wasSubscribed = this.subscribed;
    try {
      if (wasSubscribed) {
        await api.delete(
          `/api/subscriptions/${encodeURIComponent(this.channelId)}`,
        );
        this.subscribed = false;
        this.syncStatus = 'pending_delete';
      } else {
        await api.post('/api/subscriptions', { channel_id: this.channelId });
        this.subscribed = true;
        this.syncStatus = 'pending_push';
      }
      this.dispatchEvent(
        new CustomEvent('hometube:subscription-changed', {
          detail: { channelId: this.channelId, subscribed: this.subscribed },
          bubbles: true,
          composed: true,
        }),
      );
      this.startPolling();
    } catch (err) {
      this.error =
        err instanceof ApiError ? String(err.body) : (err as Error).message;
    } finally {
      this.busy = false;
    }
  }

  private startPolling(): void {
    this.stopPolling();
    let attempts = 0;
    this.pollTimer = window.setInterval(() => {
      attempts++;
      void (async () => {
        try {
          const subs = await api.get<SubscriptionRow[]>('/api/subscriptions');
          const match = subs.find((s) => s.channel_id === this.channelId);
          this.syncStatus = match ? match.sync_status : null;
          if (
            !match ||
            match.sync_status === 'synced' ||
            match.sync_status === 'error'
          ) {
            this.stopPolling();
          }
        } catch {
          // ignore
        }
      })();
      if (attempts >= POLL_MAX_ATTEMPTS) this.stopPolling();
    }, POLL_INTERVAL_MS);
  }

  private stopPolling(): void {
    if (this.pollTimer != null) {
      window.clearInterval(this.pollTimer);
      this.pollTimer = null;
    }
  }

  override render() {
    const pending =
      this.syncStatus === 'pending_push' ||
      this.syncStatus === 'pending_delete';
    const label = this.subscribed ? 'Subscribed' : 'Subscribe';
    return html`
      <button
        type="button"
        class=${this.subscribed ? 'subscribed' : ''}
        ?disabled=${this.busy}
        aria-pressed=${this.subscribed}
        @click=${this.onToggle}
      >
        ${pending
          ? html`<span class="pending-indicator" aria-hidden="true"></span>`
          : null}
        ${label}
      </button>
      ${this.error
        ? html`<div class="error" role="alert">${this.error}</div>`
        : null}
    `;
  }
}

declare global {
  interface HTMLElementTagNameMap {
    'hometube-subscribe-button': SubscribeButton;
  }
}
