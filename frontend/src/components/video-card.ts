/**
 * <hometube-video-card>
 *
 * A single thumbnail tile used by `<hometube-video-row>` and search
 * results. Renders a thumbnail, title, channel name, and an optional
 * duration badge. The whole card is one anchor (`<a>`) targeted at
 * `/child/video/:videoId` so it's keyboard-reachable for free.
 */

import { LitElement, html, css } from "lit";
import { customElement, property } from "lit/decorators.js";

@customElement("hometube-video-card")
export class VideoCard extends LitElement {
  @property({ type: String, attribute: "video-id" })
  videoId = "";

  @property({ type: String })
  title = "";

  @property({ type: String, attribute: "thumbnail-url" })
  thumbnailUrl: string | null = null;

  @property({ type: String, attribute: "channel-title" })
  channelTitle: string | null = null;

  /** Duration in seconds. Renders as a "M:SS" / "H:MM:SS" badge. */
  @property({ type: Number })
  duration: number | null = null;

  /** 0..1 progress indicator (continue-watching). */
  @property({ type: Number })
  progress = 0;

  static styles = css`
    :host {
      display: block;
    }
    a {
      display: flex;
      flex-direction: column;
      gap: 0.5rem;
      text-decoration: none;
      color: inherit;
      border-radius: 0.5rem;
      padding: 0.5rem;
    }
    a:hover,
    a:focus-visible {
      background: var(--wa-color-surface-raised);
      outline: none;
    }
    .thumb {
      position: relative;
      aspect-ratio: 16 / 9;
      border-radius: 0.375rem;
      overflow: hidden;
      background: var(--wa-color-surface-border);
    }
    .thumb img {
      width: 100%;
      height: 100%;
      object-fit: cover;
      display: block;
    }
    .duration {
      position: absolute;
      bottom: 0.25rem;
      right: 0.25rem;
      padding: 0.125rem 0.375rem;
      border-radius: 0.25rem;
      background: rgba(0, 0, 0, 0.75);
      color: white;
      font-size: 0.75rem;
    }
    .progress {
      position: absolute;
      left: 0;
      right: 0;
      bottom: 0;
      height: 0.25rem;
      background: rgba(255, 255, 255, 0.3);
    }
    .progress > span {
      display: block;
      height: 100%;
      background: var(--wa-color-brand-fill, #2563eb);
    }
    .title {
      font-weight: 600;
      font-size: 0.95rem;
      line-height: 1.3;
      overflow: hidden;
      display: -webkit-box;
      -webkit-line-clamp: 2;
      -webkit-box-orient: vertical;
    }
    .channel {
      font-size: 0.85rem;
      color: var(--wa-color-text-quiet);
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

  override render() {
    const href = this.videoId ? `/child/video/${this.videoId}` : "#";
    const pct = Math.max(0, Math.min(100, this.progress * 100));
    return html`
      <a href=${href} aria-label=${this.title}>
        <div class="thumb">
          ${this.thumbnailUrl ? html`<img src=${this.thumbnailUrl} alt="" loading="lazy" />` : null}
          ${this.duration != null
            ? html`<span class="duration">${this.formatDuration(this.duration)}</span>`
            : null}
          ${pct > 0
            ? html`<div class="progress">
                <span style="width: ${pct}%"></span>
              </div>`
            : null}
        </div>
        <div class="title">${this.title}</div>
        ${this.channelTitle ? html`<div class="channel">${this.channelTitle}</div>` : null}
      </a>
    `;
  }
}

declare global {
  interface HTMLElementTagNameMap {
    "hometube-video-card": VideoCard;
  }
}
