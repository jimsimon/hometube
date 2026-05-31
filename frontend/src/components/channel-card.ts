/**
 * <hometube-channel-card>
 *
 * Compact tile for a single channel: thumbnail + title, plus an
 * optional <hometube-subscribe-button>. The whole card is wrapped in
 * an anchor pointing at /child/channel/:id so it's keyboard-accessible.
 */

import { LitElement, html, css } from "lit";
import { customElement, property } from "lit/decorators.js";

import { normalizeThumbnailUrl } from "../types/index.js";

import "./subscribe-button.js";

@customElement("hometube-channel-card")
export class ChannelCard extends LitElement {
  @property({ type: String, attribute: "channel-id" })
  channelId = "";

  @property({ type: String })
  title = "";

  @property({ type: String, attribute: "thumbnail-url" })
  thumbnailUrl: string | null = null;

  @property({ type: Boolean, attribute: "show-subscribe" })
  showSubscribe = false;

  @property({ type: Boolean })
  hidden = false;

  static styles = css`
    :host {
      display: block;
    }
    .card {
      display: flex;
      flex-direction: column;
      gap: 0.5rem;
      padding: 0.75rem;
      border: 1px solid var(--wa-color-surface-border);
      border-radius: 0.5rem;
      background: var(--wa-color-surface-default);
    }
    :host([hidden]) .card {
      opacity: 0.55;
    }
    a {
      display: flex;
      flex-direction: column;
      align-items: center;
      gap: 0.5rem;
      text-decoration: none;
      color: inherit;
      border-radius: 0.5rem;
      padding: 0.25rem;
    }
    a:hover,
    a:focus-visible {
      background: var(--wa-color-surface-raised);
      outline: none;
    }
    img {
      width: 5rem;
      height: 5rem;
      border-radius: 50%;
      object-fit: cover;
      background: var(--wa-color-surface-border);
    }
    .placeholder {
      width: 5rem;
      height: 5rem;
      border-radius: 50%;
      background: var(--wa-color-surface-border);
    }
    .title {
      font-weight: 600;
      text-align: center;
      line-height: 1.3;
    }
    .hidden-note {
      font-size: 0.8rem;
      color: var(--wa-color-text-quiet);
      font-style: italic;
      text-align: center;
    }
  `;

  override render() {
    const href = this.channelId ? `/child/channel/${encodeURIComponent(this.channelId)}` : "#";
    const thumbSrc = normalizeThumbnailUrl(this.thumbnailUrl);
    return html`
      <div class="card">
        <a href=${href} aria-label=${this.title}>
          ${thumbSrc
            ? html`<img src=${thumbSrc} alt="" loading="lazy" />`
            : html`<div class="placeholder" aria-hidden="true"></div>`}
          <div class="title">${this.title || "Channel"}</div>
        </a>
        ${this.hidden
          ? html`<p class="hidden-note">Not on your allowlist — ask a parent to add it.</p>`
          : null}
        ${this.showSubscribe && this.channelId
          ? html`<hometube-subscribe-button
              channel-id=${this.channelId}
            ></hometube-subscribe-button>`
          : null}
      </div>
    `;
  }
}

declare global {
  interface HTMLElementTagNameMap {
    "hometube-channel-card": ChannelCard;
  }
}
