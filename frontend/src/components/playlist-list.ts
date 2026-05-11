/**
 * <hometube-playlist-list>
 *
 * Page-level component for /child/playlists. Lists the child's
 * playlists (own + library imports) and surfaces a "create playlist"
 * button that opens <hometube-create-playlist-dialog>.
 */

import { LitElement, html, css, nothing } from 'lit';
import { customElement, query, state } from 'lit/decorators.js';

import { api } from '../services/api.js';
import type {
  FamilyPlaylistSummary,
  PlaylistSummary,
} from '../types/index.js';

import './playlist-card.js';
import './create-playlist-dialog.js';
import type { CreatePlaylistDialog } from './create-playlist-dialog.js';

@customElement('hometube-playlist-list')
export class PlaylistList extends LitElement {
  @state() private playlists: PlaylistSummary[] = [];
  @state() private familyPlaylists: FamilyPlaylistSummary[] = [];
  @state() private loading = false;
  @state() private error = '';

  @query('hometube-create-playlist-dialog')
  private dialog!: CreatePlaylistDialog;

  static styles = css`
    :host {
      display: block;
    }
    .toolbar {
      display: flex;
      gap: 0.5rem;
      margin-block: 1rem;
    }
    button {
      padding: 0.5rem 1rem;
      border-radius: 0.375rem;
      border: 1px solid var(--wa-color-surface-border);
      background: var(--wa-color-brand-fill, #2563eb);
      color: white;
      font: inherit;
      cursor: pointer;
    }
    h2 {
      margin: 1.25rem 0 0.5rem;
      font-size: 1.1rem;
      color: var(--wa-color-text-quiet);
    }
    .grid {
      display: grid;
      gap: 1rem;
      grid-template-columns: repeat(auto-fill, minmax(min(16rem, 100%), 1fr));
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
      'hometube:playlist-created',
      this.onCreated as EventListener,
    );
  }

  override disconnectedCallback(): void {
    super.disconnectedCallback();
    this.removeEventListener(
      'hometube:playlist-created',
      this.onCreated as EventListener,
    );
  }

  private onCreated = (): void => {
    void this.load();
  };

  private async load(): Promise<void> {
    this.loading = true;
    this.error = '';
    try {
      const [own, family] = await Promise.all([
        api.get<PlaylistSummary[]>('/api/playlists'),
        api
          .get<FamilyPlaylistSummary[]>('/api/family-playlists')
          .catch(() => [] as FamilyPlaylistSummary[]),
      ]);
      this.playlists = own;
      this.familyPlaylists = family;
    } catch (err) {
      this.error = (err as Error).message;
    } finally {
      this.loading = false;
    }
  }

  private openCreate = (): void => {
    this.dialog?.open();
  };

  override render() {
    if (this.loading) return html`<p class="empty">Loading…</p>`;
    if (this.error) return html`<p class="error" role="alert">${this.error}</p>`;
    const own = this.playlists.filter((p) => p.is_own);
    const library = this.playlists.filter((p) => !p.is_own);
    return html`
      <div class="toolbar">
        <button type="button" @click=${this.openCreate}>
          New playlist
        </button>
      </div>

      ${own.length > 0
        ? html`
            <h2>Your playlists</h2>
            <div class="grid" role="list">
              ${own.map(
                (p) => html`
                  <hometube-playlist-card
                    role="listitem"
                    playlist-id=${p.id}
                    title=${p.title}
                    video-count=${p.video_count}
                    sync-status=${p.sync_status}
                    ?is-own=${true}
                  ></hometube-playlist-card>
                `,
              )}
            </div>
          `
        : nothing}
      ${library.length > 0
        ? html`
            <h2>From your library</h2>
            <div class="grid" role="list">
              ${library.map(
                (p) => html`
                  <hometube-playlist-card
                    role="listitem"
                    playlist-id=${p.id}
                    title=${p.title}
                    video-count=${p.video_count}
                    sync-status=${p.sync_status}
                  ></hometube-playlist-card>
                `,
              )}
            </div>
          `
        : nothing}
      ${this.familyPlaylists.length > 0
        ? html`
            <h2>Shared with you</h2>
            <div class="grid" role="list">
              ${this.familyPlaylists.map(
                (p) => html`
                  <a
                    role="listitem"
                    class="empty"
                    style="display: grid; gap: 0.25rem; padding: 0.75rem; border: 1px solid var(--wa-color-surface-border); border-radius: 0.5rem; background: var(--wa-color-surface-default); color: var(--wa-color-text-normal); text-decoration: none;"
                    href=${`/child/playlist/family:${p.id}`}
                  >
                    <strong style="font-size: 1.05rem; font-style: normal;"
                      >${p.title}</strong
                    >
                    <span style="font-size: 0.85rem;"
                      >${p.video_count} videos · shared by family</span
                    >
                  </a>
                `,
              )}
            </div>
          `
        : nothing}
      ${own.length === 0 &&
      library.length === 0 &&
      this.familyPlaylists.length === 0
        ? html`<p class="empty">
            You don't have any playlists yet. Tap "New playlist" to start.
          </p>`
        : nothing}

      <hometube-create-playlist-dialog></hometube-create-playlist-dialog>
    `;
  }
}

declare global {
  interface HTMLElementTagNameMap {
    'hometube-playlist-list': PlaylistList;
  }
}
