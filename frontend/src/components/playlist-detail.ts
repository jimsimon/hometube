/**
 * <hometube-playlist-detail playlist-id="...">
 *
 * Detail view for /child/playlist/:id. Loads /api/playlists/:id, shows
 * the title + description + sync status, and renders the videos as a
 * reorderable list (HTML5 drag-and-drop) when the playlist is owned by
 * the child. Library playlists are read-only.
 *
 * Reorder UX: each <li> is `draggable=true`. On `drop`, the local
 * order is updated optimistically and the new order is PUT to
 * /api/playlists/:id/videos/reorder. Failures show an error message
 * but don't roll back — the next refresh will reconcile.
 */

import { LitElement, html, css, nothing } from 'lit';
import { customElement, property, state } from 'lit/decorators.js';

import { ApiError, api } from '../services/api.js';
import type { PlaylistDetail, PlaylistVideo } from '../types/index.js';

@customElement('hometube-playlist-detail')
export class PlaylistDetailEl extends LitElement {
  @property({ type: Number, attribute: 'playlist-id' })
  playlistId = 0;

  @state() private detail: PlaylistDetail | null = null;
  @state() private loading = false;
  @state() private error = '';
  @state() private dragIndex: number | null = null;

  static styles = css`
    :host {
      display: block;
    }
    header {
      display: flex;
      gap: 1rem;
      align-items: flex-start;
      padding: 1rem;
      border-bottom: 1px solid var(--wa-color-surface-border);
    }
    .meta {
      flex: 1;
    }
    .meta h1 {
      margin: 0 0 0.25rem;
      font-size: 1.4rem;
    }
    .meta .description {
      color: var(--wa-color-text-quiet);
      white-space: pre-wrap;
    }
    .meta .stats {
      color: var(--wa-color-text-quiet);
      font-size: 0.9rem;
      margin-top: 0.25rem;
    }
    .badge {
      display: inline-block;
      font-size: 0.75rem;
      padding: 0.15rem 0.5rem;
      border-radius: 999px;
      background: var(--wa-color-surface-raised);
      color: var(--wa-color-text-quiet);
      margin-right: 0.25rem;
    }
    .badge.pending {
      background: var(--wa-color-warning-quiet, rgba(217, 119, 6, 0.15));
      color: var(--wa-color-warning-on-quiet, #92400e);
    }
    .badge.error {
      background: var(--wa-color-danger-quiet, rgba(185, 28, 28, 0.15));
      color: var(--wa-color-danger-on-quiet, #991b1b);
    }
    ol {
      list-style: none;
      padding: 0;
      margin: 0;
      display: grid;
      gap: 0.5rem;
    }
    li {
      display: grid;
      grid-template-columns: auto auto 1fr auto;
      gap: 0.75rem;
      align-items: center;
      padding: 0.5rem;
      border: 1px solid var(--wa-color-surface-border);
      border-radius: 0.375rem;
      background: var(--wa-color-surface-default);
    }
    li.dragging {
      opacity: 0.55;
    }
    .grip {
      cursor: grab;
      padding: 0.25rem 0.5rem;
      color: var(--wa-color-text-quiet);
      user-select: none;
    }
    li[aria-disabled='true'] .grip {
      cursor: not-allowed;
      opacity: 0.5;
    }
    img {
      width: 8rem;
      height: 4.5rem;
      object-fit: cover;
      border-radius: 0.25rem;
      background: var(--wa-color-surface-border);
    }
    .row-meta {
      min-width: 0;
    }
    .row-title {
      font-weight: 600;
      overflow: hidden;
      display: -webkit-box;
      -webkit-line-clamp: 2;
      -webkit-box-orient: vertical;
    }
    .row-channel {
      font-size: 0.85rem;
      color: var(--wa-color-text-quiet);
    }
    .actions {
      display: flex;
      gap: 0.25rem;
    }
    button {
      padding: 0.35rem 0.6rem;
      border-radius: 0.375rem;
      border: 1px solid var(--wa-color-surface-border);
      background: transparent;
      color: var(--wa-color-text-normal);
      font: inherit;
      cursor: pointer;
    }
    .empty {
      color: var(--wa-color-text-quiet);
      font-style: italic;
      padding: 1rem;
    }
    .error {
      color: var(--wa-color-danger-fill, #b91c1c);
      padding: 1rem;
    }
    a {
      color: inherit;
      text-decoration: none;
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
      this.detail = await api.get<PlaylistDetail>(
        `/api/playlists/${this.playlistId}`,
      );
    } catch (err) {
      this.error =
        err instanceof ApiError ? String(err.body) : (err as Error).message;
    } finally {
      this.loading = false;
    }
  }

  private onDragStart = (index: number) => (e: DragEvent) => {
    if (!this.detail?.is_own) return;
    this.dragIndex = index;
    if (e.dataTransfer) {
      e.dataTransfer.effectAllowed = 'move';
      e.dataTransfer.setData('text/plain', String(index));
    }
  };

  private onDragOver = (e: DragEvent): void => {
    if (!this.detail?.is_own) return;
    e.preventDefault();
    if (e.dataTransfer) e.dataTransfer.dropEffect = 'move';
  };

  private onDrop = (targetIndex: number) => (e: DragEvent) => {
    e.preventDefault();
    if (!this.detail?.is_own) return;
    if (this.dragIndex == null || this.dragIndex === targetIndex) {
      this.dragIndex = null;
      return;
    }
    const videos = [...this.detail.videos];
    const [moved] = videos.splice(this.dragIndex, 1);
    if (moved) videos.splice(targetIndex, 0, moved);
    this.detail = { ...this.detail, videos };
    this.dragIndex = null;
    void this.pushReorder(videos);
  };

  private async pushReorder(videos: PlaylistVideo[]): Promise<void> {
    try {
      await api.put(`/api/playlists/${this.playlistId}/videos/reorder`, {
        video_ids: videos.map((v) => v.video_id),
      });
    } catch (err) {
      this.error =
        err instanceof ApiError ? String(err.body) : (err as Error).message;
      void this.load();
    }
  }

  private async onRemove(videoId: string): Promise<void> {
    if (!this.detail?.is_own) return;
    try {
      await api.delete(
        `/api/playlists/${this.playlistId}/videos/${encodeURIComponent(videoId)}`,
      );
      await this.load();
    } catch (err) {
      this.error = (err as Error).message;
    }
  }

  override render() {
    if (this.loading) return html`<p class="empty">Loading…</p>`;
    if (this.error) return html`<p class="error" role="alert">${this.error}</p>`;
    if (!this.detail) return null;

    const isPending = this.detail.sync_status.startsWith('pending');
    const isError = this.detail.sync_status === 'error';

    return html`
      <header>
        <div class="meta">
          <h1>${this.detail.title}</h1>
          ${this.detail.description
            ? html`<p class="description">${this.detail.description}</p>`
            : nothing}
          <p class="stats">${this.detail.video_count} videos</p>
          <span class="badge ${this.detail.is_own ? '' : ''}"
            >${this.detail.is_own ? 'Yours' : 'Library'}</span
          >
          ${isPending
            ? html`<span class="badge pending">Syncing…</span>`
            : nothing}
          ${isError ? html`<span class="badge error">Sync error</span>` : nothing}
        </div>
      </header>

      ${this.detail.videos.length === 0
        ? html`<p class="empty">No videos in this playlist yet.</p>`
        : html`
            <ol aria-label="Playlist videos">
              ${this.detail.videos.map(
                (v, i) => html`
                  <li
                    draggable=${this.detail!.is_own}
                    aria-disabled=${!this.detail!.is_own}
                    class=${this.dragIndex === i ? 'dragging' : ''}
                    @dragstart=${this.onDragStart(i)}
                    @dragover=${this.onDragOver}
                    @drop=${this.onDrop(i)}
                  >
                    <span class="grip" aria-hidden="true">⋮⋮</span>
                    ${v.video_thumbnail_url
                      ? html`<img src=${v.video_thumbnail_url} alt="" />`
                      : html`<div
                          class="placeholder"
                          aria-hidden="true"
                        ></div>`}
                    <a
                      href="/child/video/${encodeURIComponent(v.video_id)}?from=playlist:${this.detail!.id}"
                      class="row-meta"
                    >
                      <div class="row-title">${v.video_title}</div>
                      ${v.channel_title
                        ? html`<div class="row-channel">${v.channel_title}</div>`
                        : nothing}
                    </a>
                    <div class="actions">
                      ${this.detail!.is_own
                        ? html`<button
                            type="button"
                            aria-label="Remove from playlist"
                            @click=${() => void this.onRemove(v.video_id)}
                          >
                            Remove
                          </button>`
                        : nothing}
                    </div>
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
    'hometube-playlist-detail': PlaylistDetailEl;
  }
}
