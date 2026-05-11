/**
 * <hometube-preview-playlist playlist-id="...">
 *
 * Parent-side playlist preview rendered on /parent/preview/playlist/:id.
 * Lists the playlist's items so a parent can scan content before
 * adding it to a child's allowlist.
 */

import { LitElement, html, css, nothing } from 'lit';
import { customElement, property, state } from 'lit/decorators.js';

import { ApiError, api } from '../services/api.js';
import { pickThumbnail, type PlaylistPreview } from '../types/index.js';

import './loading-spinner.js';
import './error-banner.js';

@customElement('hometube-preview-playlist')
export class PreviewPlaylist extends LitElement {
  @property({ type: String, attribute: 'playlist-id' })
  playlistId = '';

  @state() private data: PlaylistPreview | null = null;
  @state() private loading = false;
  @state() private error = '';

  static styles = css`
    :host {
      display: block;
    }
    header {
      padding: 1rem;
      border: 1px solid var(--wa-color-surface-border);
      border-radius: 0.5rem;
      background: var(--wa-color-surface-default);
    }
    h1 {
      margin: 0 0 0.25rem;
    }
    .description {
      color: var(--wa-color-text-quiet);
      white-space: pre-wrap;
      font-size: 0.95rem;
    }
    .grid {
      margin-top: 1rem;
      display: grid;
      gap: 0.75rem;
      grid-template-columns: repeat(auto-fill, minmax(min(14rem, 100%), 1fr));
    }
    .card {
      display: grid;
      gap: 0.25rem;
      padding: 0.5rem;
      border: 1px solid var(--wa-color-surface-border);
      border-radius: 0.5rem;
      background: var(--wa-color-surface-default);
      text-decoration: none;
      color: inherit;
    }
    .card img {
      width: 100%;
      aspect-ratio: 16 / 9;
      object-fit: cover;
      border-radius: 0.375rem;
      background: var(--wa-color-surface-border);
    }
    .card .title {
      font-weight: 600;
      font-size: 0.95rem;
      overflow: hidden;
      display: -webkit-box;
      -webkit-line-clamp: 2;
      -webkit-box-orient: vertical;
    }
  `;

  override connectedCallback(): void {
    super.connectedCallback();
    if (this.playlistId) void this.load();
  }

  override updated(changed: Map<string, unknown>): void {
    if (changed.has('playlistId') && this.playlistId) {
      void this.load();
    }
  }

  private async load(): Promise<void> {
    this.loading = true;
    this.error = '';
    try {
      this.data = await api.get<PlaylistPreview>(
        `/api/preview/playlist/${encodeURIComponent(this.playlistId)}`,
      );
    } catch (err) {
      this.error =
        err instanceof ApiError ? String(err.body) : (err as Error).message;
    } finally {
      this.loading = false;
    }
  }

  override render() {
    if (this.loading && !this.data) {
      return html`<hometube-loading-spinner></hometube-loading-spinner>`;
    }
    if (this.error) {
      return html`<hometube-error-banner
        message=${this.error}
      ></hometube-error-banner>`;
    }
    if (!this.data) return nothing;

    return html`
      <header>
        <h1>${this.data.title}</h1>
        ${this.data.channel_title
          ? html`<div class="description">${this.data.channel_title}</div>`
          : nothing}
        ${this.data.description
          ? html`<p class="description">${this.data.description}</p>`
          : nothing}
      </header>

      <div class="grid">
        ${this.data.videos.map(
          (v) => html`
            <a
              class="card"
              href="/parent/preview/video/${encodeURIComponent(v.video_id)}"
            >
              ${pickThumbnail(v.thumbnails)
                ? html`<img src=${pickThumbnail(v.thumbnails)!} alt="" />`
                : nothing}
              <div class="title">${v.title}</div>
            </a>
          `,
        )}
      </div>
    `;
  }
}

declare global {
  interface HTMLElementTagNameMap {
    'hometube-preview-playlist': PreviewPlaylist;
  }
}
