/**
 * <hometube-family-playlist-view playlist-id="...">
 *
 * Read-only child-facing rendering of a family playlist. The parent's
 * <hometube-family-playlist-detail> handles editing — children just
 * see the videos and can tap through to play them.
 */

import { LitElement, html, css, nothing } from 'lit';
import { customElement, property, state } from 'lit/decorators.js';

import { ApiError, api } from '../services/api.js';
import type { FamilyPlaylistDetail } from '../types/index.js';

import './loading-spinner.js';
import './error-banner.js';

@customElement('hometube-family-playlist-view')
export class FamilyPlaylistView extends LitElement {
  @property({ type: String, attribute: 'playlist-id' })
  playlistId = '';

  @state() private detail: FamilyPlaylistDetail | null = null;
  @state() private loading = false;
  @state() private error = '';

  static styles = css`
    :host {
      display: block;
    }
    header {
      padding: 1rem;
      border-bottom: 1px solid var(--wa-color-surface-border);
    }
    h1 {
      margin: 0 0 0.25rem;
      font-size: 1.4rem;
    }
    .description {
      color: var(--wa-color-text-quiet);
      white-space: pre-wrap;
    }
    .badge {
      display: inline-block;
      padding: 0.15rem 0.5rem;
      border-radius: 999px;
      background: var(--wa-color-brand-quiet, rgba(37, 99, 235, 0.1));
      color: var(--wa-color-brand-on-quiet, #1d4ed8);
      font-size: 0.75rem;
    }
    ol {
      list-style: none;
      padding: 0;
      margin: 0;
      display: grid;
      gap: 0.5rem;
    }
    li a {
      display: grid;
      grid-template-columns: auto 1fr;
      gap: 0.75rem;
      align-items: center;
      padding: 0.5rem;
      border: 1px solid var(--wa-color-surface-border);
      border-radius: 0.375rem;
      background: var(--wa-color-surface-default);
      color: inherit;
      text-decoration: none;
    }
    li a:focus-visible {
      outline: 2px solid var(--wa-color-brand-fill, #2563eb);
      outline-offset: 2px;
    }
    img {
      width: 8rem;
      height: 4.5rem;
      object-fit: cover;
      border-radius: 0.25rem;
      background: var(--wa-color-surface-border);
    }
    .row-title {
      font-weight: 600;
    }
    .row-channel {
      color: var(--wa-color-text-quiet);
      font-size: 0.85rem;
    }
    .empty {
      color: var(--wa-color-text-quiet);
      font-style: italic;
      padding: 1rem;
    }
  `;

  override connectedCallback(): void {
    super.connectedCallback();
    void this.load();
  }

  private async load(): Promise<void> {
    this.loading = true;
    this.error = '';
    try {
      this.detail = await api.get<FamilyPlaylistDetail>(
        `/api/family-playlists/${encodeURIComponent(this.playlistId)}`,
      );
    } catch (err) {
      this.error =
        err instanceof ApiError ? String(err.body) : (err as Error).message;
    } finally {
      this.loading = false;
    }
  }

  override render() {
    if (this.loading && !this.detail) {
      return html`<hometube-loading-spinner></hometube-loading-spinner>`;
    }
    if (this.error) {
      return html`<hometube-error-banner
        message=${this.error}
      ></hometube-error-banner>`;
    }
    if (!this.detail) return nothing;

    return html`
      <header>
        <h1>${this.detail.title}</h1>
        <span class="badge">Shared with you</span>
        ${this.detail.description
          ? html`<p class="description">${this.detail.description}</p>`
          : nothing}
      </header>

      ${this.detail.videos.length === 0
        ? html`<p class="empty">No videos in this playlist yet.</p>`
        : html`
            <ol aria-label="Playlist videos">
              ${this.detail.videos.map(
                (v) => html`
                  <li>
                    <a
                      href="/child/video/${encodeURIComponent(v.video_id)}?from=family:${this.detail!.id}"
                    >
                      ${v.video_thumbnail_url
                        ? html`<img src=${v.video_thumbnail_url} alt="" />`
                        : html`<div></div>`}
                      <div>
                        <div class="row-title">${v.video_title}</div>
                        ${v.channel_title
                          ? html`<div class="row-channel">
                              ${v.channel_title}
                            </div>`
                          : nothing}
                      </div>
                    </a>
                  </li>
                `,
              )}
            </ol>
          `}
    `;
  }
}

declare global {
  interface HTMLElementTagNameMap {
    'hometube-family-playlist-view': FamilyPlaylistView;
  }
}
