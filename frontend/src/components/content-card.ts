/**
 * <hometube-content-card>
 *
 * Unified content card component supporting two layout variants:
 *   - "full" (default): vertical layout with 16:9 thumbnail, title,
 *     channel, and optional duration badge. Used in grids.
 *   - "compact": horizontal layout with smaller thumbnail (8rem × 4.5rem)
 *     alongside title and meta text. Used in list contexts (up-next,
 *     allowlist manager).
 *
 * Supports a default `<slot>` for action buttons (e.g., "Remove",
 * "Preview", "Add to allowlist") rendered below the card meta.
 *
 * When `href` is set the card is wrapped in a link; otherwise it's a
 * plain presentational container with slotted actions.
 */

import { LitElement, html, css, nothing } from "lit";
import { customElement, property } from "lit/decorators.js";

@customElement("hometube-content-card")
export class ContentCard extends LitElement {
  @property({ type: String })
  variant: "full" | "compact" = "full";

  @property({ type: String, attribute: "content-id" })
  contentId = "";

  @property({ type: String })
  override title = "";

  @property({ type: String, attribute: "thumbnail-url" })
  thumbnailUrl: string | null = null;

  @property({ type: String, attribute: "channel-title" })
  channelTitle: string | null = null;

  @property({ type: Number })
  duration: number | null = null;

  @property({ type: String })
  href: string | null = null;

  static styles = css`
    :host {
      display: block;
    }

    /* ─── Full variant (vertical, grid card) ─── */
    .full {
      display: flex;
      flex-direction: column;
      gap: 0.5rem;
      padding: 0.5rem;
      border-radius: 0.5rem;
      border: 1px solid var(--wa-color-surface-border);
      background: var(--wa-color-surface-default);
    }
    .full .thumb {
      position: relative;
      aspect-ratio: 16 / 9;
      border-radius: 0.375rem;
      overflow: hidden;
      background: var(--wa-color-surface-border);
    }
    .full .thumb img {
      width: 100%;
      height: 100%;
      object-fit: cover;
      display: block;
    }
    .full .duration {
      position: absolute;
      bottom: 0.25rem;
      right: 0.25rem;
      padding: 0.125rem 0.375rem;
      border-radius: 0.25rem;
      background: rgba(0, 0, 0, 0.75);
      color: white;
      font-size: 0.75rem;
    }
    .full .title {
      font-weight: 600;
      font-size: 0.95rem;
      line-height: 1.3;
      overflow: hidden;
      display: -webkit-box;
      -webkit-line-clamp: 2;
      -webkit-box-orient: vertical;
    }
    .full .channel {
      font-size: 0.85rem;
      color: var(--wa-color-text-quiet);
    }

    /* ─── Compact variant (horizontal, list item) ─── */
    .compact {
      display: flex;
      gap: 0.75rem;
      padding: 0.75rem;
      border: 1px solid var(--wa-color-surface-border);
      border-radius: 0.5rem;
      background: var(--wa-color-surface-default);
    }
    .compact .thumb {
      flex-shrink: 0;
      width: 8rem;
      height: 4.5rem;
      border-radius: 0.25rem;
      overflow: hidden;
      background: var(--wa-color-surface-border);
    }
    .compact .thumb img {
      width: 100%;
      height: 100%;
      object-fit: cover;
      display: block;
    }
    .compact .meta {
      display: flex;
      flex-direction: column;
      gap: 0.25rem;
      min-width: 0;
      flex: 1;
    }
    .compact .title {
      font-weight: 600;
      font-size: 0.95rem;
      line-height: 1.3;
      overflow: hidden;
      display: -webkit-box;
      -webkit-line-clamp: 2;
      -webkit-box-orient: vertical;
    }
    .compact .channel {
      font-size: 0.85rem;
      color: var(--wa-color-text-quiet);
    }
    .compact .duration {
      font-size: 0.8rem;
      color: var(--wa-color-text-quiet);
    }

    /* ─── Shared ─── */
    a {
      text-decoration: none;
      color: inherit;
      display: contents;
    }
    .full:hover,
    .compact:hover {
      background: var(--wa-color-surface-raised);
    }
    ::slotted(*) {
      margin-top: 0.25rem;
    }
  `;

  private formatDuration(seconds: number): string {
    const s = Math.max(0, Math.round(seconds));
    const h = Math.floor(s / 3600);
    const m = Math.floor((s % 3600) / 60);
    const sec = s % 60;
    if (h > 0) return `${h}:${String(m).padStart(2, "0")}:${String(sec).padStart(2, "0")}`;
    return `${m}:${String(sec).padStart(2, "0")}`;
  }

  private renderThumb() {
    return html`
      <div class="thumb">
        ${this.thumbnailUrl
          ? html`<img src=${this.thumbnailUrl} alt="" loading="lazy" />`
          : nothing}
        ${this.variant === "full" && this.duration != null
          ? html`<span class="duration">${this.formatDuration(this.duration)}</span>`
          : nothing}
      </div>
    `;
  }

  override render() {
    const content =
      this.variant === "compact"
        ? html`
            <div class="compact">
              ${this.renderThumb()}
              <div class="meta">
                <div class="title">${this.title}</div>
                ${this.channelTitle
                  ? html`<div class="channel">${this.channelTitle}</div>`
                  : nothing}
                ${this.duration != null
                  ? html`<div class="duration">${this.formatDuration(this.duration)}</div>`
                  : nothing}
                <slot></slot>
              </div>
            </div>
          `
        : html`
            <div class="full">
              ${this.renderThumb()}
              <div class="title">${this.title}</div>
              ${this.channelTitle ? html`<div class="channel">${this.channelTitle}</div>` : nothing}
              <slot></slot>
            </div>
          `;

    if (this.href) {
      return html`<a href=${this.href} aria-label=${this.title || nothing}>${content}</a>`;
    }
    return content;
  }
}

declare global {
  interface HTMLElementTagNameMap {
    "hometube-content-card": ContentCard;
  }
}
