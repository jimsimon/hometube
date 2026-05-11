/**
 * <hometube-channel-list>
 *
 * Renders the child's subscribed channels: visible (allowlisted) ones
 * up top, hidden (not yet allowlisted) ones in a separate "Pending"
 * section so the child can see what they've subscribed to that the
 * parent hasn't approved yet.
 */

import { LitElement, html, css, nothing } from 'lit';
import { customElement, state } from 'lit/decorators.js';

import { api } from '../services/api.js';
import type { SubscriptionRow } from '../types/index.js';

import './channel-card.js';

@customElement('hometube-channel-list')
export class ChannelList extends LitElement {
  @state() private subs: SubscriptionRow[] = [];
  @state() private loading = false;
  @state() private error = '';

  static styles = css`
    :host {
      display: block;
    }
    .grid {
      display: grid;
      gap: 1rem;
      grid-template-columns: repeat(auto-fill, minmax(min(12rem, 100%), 1fr));
      margin-block: 1rem;
    }
    h2 {
      margin: 1.5rem 0 0.5rem;
      font-size: 1.1rem;
      color: var(--wa-color-text-quiet);
    }
    .empty {
      color: var(--wa-color-text-quiet);
      font-style: italic;
    }
    .error {
      color: var(--wa-color-danger-fill, #b91c1c);
    }
  `;

  override connectedCallback(): void {
    super.connectedCallback();
    void this.load();
    this.addEventListener(
      'hometube:subscription-changed',
      this.onChanged as EventListener,
    );
  }

  override disconnectedCallback(): void {
    super.disconnectedCallback();
    this.removeEventListener(
      'hometube:subscription-changed',
      this.onChanged as EventListener,
    );
  }

  private onChanged = (): void => {
    void this.load();
  };

  private async load(): Promise<void> {
    this.loading = true;
    this.error = '';
    try {
      this.subs = await api.get<SubscriptionRow[]>('/api/subscriptions');
    } catch (err) {
      this.error = (err as Error).message;
    } finally {
      this.loading = false;
    }
  }

  override render() {
    if (this.loading) return html`<p class="empty">Loading…</p>`;
    if (this.error) {
      return html`<p class="error" role="alert">${this.error}</p>`;
    }
    if (this.subs.length === 0) {
      return html`<p class="empty">
        You haven't subscribed to any channels yet.
      </p>`;
    }
    const visible = this.subs.filter((s) => s.visible);
    const hidden = this.subs.filter((s) => !s.visible);
    return html`
      ${visible.length > 0
        ? html`
            <div class="grid" role="list">
              ${visible.map(
                (s) => html`
                  <hometube-channel-card
                    role="listitem"
                    channel-id=${s.channel_id}
                    title=${s.channel_title}
                    thumbnail-url=${s.channel_thumbnail_url ?? ''}
                  ></hometube-channel-card>
                `,
              )}
            </div>
          `
        : nothing}
      ${hidden.length > 0
        ? html`
            <h2>Waiting for parent approval</h2>
            <div class="grid" role="list">
              ${hidden.map(
                (s) => html`
                  <hometube-channel-card
                    role="listitem"
                    channel-id=${s.channel_id}
                    title=${s.channel_title}
                    thumbnail-url=${s.channel_thumbnail_url ?? ''}
                    hidden
                  ></hometube-channel-card>
                `,
              )}
            </div>
          `
        : nothing}
    `;
  }
}

declare global {
  interface HTMLElementTagNameMap {
    'hometube-channel-list': ChannelList;
  }
}
