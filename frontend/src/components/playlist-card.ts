/**
 * <hometube-playlist-card>
 *
 * Tile representing a single playlist on the playlists list page. Shows
 * the title, video count, and ownership badge ("yours" vs "library").
 */

import { LitElement, html, css } from "lit";
import { customElement, property } from "lit/decorators.js";

@customElement("hometube-playlist-card")
export class PlaylistCard extends LitElement {
  @property({ type: Number, attribute: "playlist-id" })
  playlistId = 0;

  @property({ type: String })
  title = "";

  @property({ type: Number, attribute: "video-count" })
  videoCount = 0;

  @property({ type: String, attribute: "thumbnail-url" })
  thumbnailUrl: string | null = null;

  @property({ type: Boolean, attribute: "is-own" })
  isOwn = true;

  static styles = css`
    :host {
      display: block;
    }
    a {
      display: flex;
      flex-direction: column;
      gap: 0.5rem;
      padding: 0.75rem;
      text-decoration: none;
      color: inherit;
      border: 1px solid var(--wa-color-surface-border);
      border-radius: 0.5rem;
      background: var(--wa-color-surface-default);
    }
    a:hover,
    a:focus-visible {
      background: var(--wa-color-surface-raised);
      outline: none;
    }
    .thumb {
      position: relative;
      aspect-ratio: 16 / 9;
      background: var(--wa-color-surface-border);
      border-radius: 0.375rem;
      overflow: hidden;
      display: flex;
      align-items: center;
      justify-content: center;
    }
    .thumb img {
      width: 100%;
      height: 100%;
      object-fit: cover;
    }
    .thumb .placeholder-icon {
      font-size: 1.5rem;
      color: var(--wa-color-text-quiet);
      opacity: 0.5;
    }
    .row {
      display: flex;
      gap: 0.5rem;
      align-items: center;
      justify-content: space-between;
      flex-wrap: wrap;
    }
    .title {
      font-weight: 600;
      line-height: 1.3;
    }
    .meta {
      font-size: 0.85rem;
      color: var(--wa-color-text-quiet);
    }
    .badge {
      font-size: 0.75rem;
      padding: 0.15rem 0.5rem;
      border-radius: 999px;
      background: var(--wa-color-surface-raised);
      color: var(--wa-color-text-quiet);
    }
    .badge.own {
      background: var(--wa-color-brand-quiet, rgba(37, 99, 235, 0.15));
      color: var(--wa-color-brand-on-quiet);
    }
  `;

  override render() {
    const href = `/child/playlist/${this.playlistId}`;
    return html`
      <a href=${href} aria-label=${this.title}>
        <div class="thumb">
          ${this.thumbnailUrl
            ? html`<img src=${this.thumbnailUrl} alt="" loading="lazy" />`
            : html`<span class="placeholder-icon" aria-hidden="true">♫</span>`}
        </div>
        <div class="title">${this.title}</div>
        <div class="row">
          <span class="meta">${this.videoCount} videos</span>
          <span class="badge ${this.isOwn ? "own" : ""}">${this.isOwn ? "Yours" : "Library"}</span>
        </div>
      </a>
    `;
  }
}

declare global {
  interface HTMLElementTagNameMap {
    "hometube-playlist-card": PlaylistCard;
  }
}
