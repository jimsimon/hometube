/**
 * <hometube-family-playlist-manager>
 *
 * Parent-side list of family playlists with create / edit / delete.
 * Lives on /parent/playlists. Each row links to /parent/playlist/:id
 * for the detail view.
 */

import { LitElement, html, css, nothing } from 'lit';
import { customElement, query, state } from 'lit/decorators.js';

import { ApiError, api } from '../services/api.js';
import type { FamilyPlaylistSummary } from '../types/index.js';

import './family-playlist-form.js';
import './loading-spinner.js';
import './error-banner.js';
import type { FamilyPlaylistForm } from './family-playlist-form.js';

@customElement('hometube-family-playlist-manager')
export class FamilyPlaylistManager extends LitElement {
  @state() private playlists: FamilyPlaylistSummary[] = [];
  @state() private loading = false;
  @state() private error = '';

  @query('hometube-family-playlist-form')
  private form!: FamilyPlaylistForm;

  static styles = css`
    :host {
      display: block;
    }
    .toolbar {
      display: flex;
      gap: 0.5rem;
      margin-block: 1rem;
    }
    button.primary {
      padding: 0.5rem 1rem;
      border-radius: 0.375rem;
      border: 1px solid transparent;
      background: var(--wa-color-brand-fill, #2563eb);
      color: white;
      font: inherit;
      cursor: pointer;
    }
    button.secondary {
      padding: 0.4rem 0.75rem;
      border-radius: 0.375rem;
      border: 1px solid var(--wa-color-surface-border, #ccc);
      background: transparent;
      color: inherit;
      font: inherit;
      cursor: pointer;
    }
    .grid {
      display: grid;
      gap: 0.75rem;
      grid-template-columns: repeat(auto-fill, minmax(min(18rem, 100%), 1fr));
    }
    .card {
      padding: 0.75rem;
      border: 1px solid var(--wa-color-surface-border);
      border-radius: 0.5rem;
      background: var(--wa-color-surface-default);
      display: grid;
      gap: 0.5rem;
    }
    .card .title {
      font-weight: 600;
      font-size: 1.05rem;
    }
    .card .description {
      color: var(--wa-color-text-quiet);
      font-size: 0.9rem;
      white-space: pre-wrap;
    }
    .card .stats {
      color: var(--wa-color-text-quiet);
      font-size: 0.85rem;
    }
    .actions {
      display: flex;
      gap: 0.25rem;
    }
    .empty {
      color: var(--wa-color-text-quiet);
      font-style: italic;
    }
    a {
      color: inherit;
      text-decoration: none;
    }
  `;

  override connectedCallback(): void {
    super.connectedCallback();
    void this.refresh();
    this.addEventListener(
      'hometube:family-playlist-saved',
      this.onSaved as EventListener,
    );
  }

  override disconnectedCallback(): void {
    super.disconnectedCallback();
    this.removeEventListener(
      'hometube:family-playlist-saved',
      this.onSaved as EventListener,
    );
  }

  private onSaved = (): void => {
    void this.refresh();
  };

  private async refresh(): Promise<void> {
    this.loading = true;
    this.error = '';
    try {
      this.playlists =
        await api.get<FamilyPlaylistSummary[]>('/api/family-playlists');
    } catch (err) {
      this.error =
        err instanceof ApiError ? String(err.body) : (err as Error).message;
    } finally {
      this.loading = false;
    }
  }

  private openCreate = (): void => {
    this.form?.open();
  };

  private async deletePlaylist(id: number): Promise<void> {
    if (!confirm('Delete this family playlist? This cannot be undone.')) {
      return;
    }
    try {
      await api.delete(`/api/family-playlists/${id}`);
      await this.refresh();
    } catch (err) {
      this.error = (err as Error).message;
    }
  }

  override render() {
    return html`
      ${this.error
        ? html`<hometube-error-banner
            message=${this.error}
            dismissible
            @hometube:error-dismiss=${() => (this.error = '')}
          ></hometube-error-banner>`
        : nothing}
      <div class="toolbar">
        <button class="primary" type="button" @click=${this.openCreate}>
          New playlist
        </button>
      </div>

      ${this.loading
        ? html`<hometube-loading-spinner
            label="Loading playlists…"
          ></hometube-loading-spinner>`
        : this.playlists.length === 0
          ? html`<p class="empty">
              No family playlists yet. Tap "New playlist" to create one.
            </p>`
          : html`
              <div class="grid">
                ${this.playlists.map(
                  (p) => html`
                    <div class="card">
                      <a href="/parent/playlist/${p.id}" class="title">
                        ${p.title}
                      </a>
                      ${p.description
                        ? html`<div class="description">
                            ${p.description}
                          </div>`
                        : nothing}
                      <div class="stats">${p.video_count} videos</div>
                      <div class="actions">
                        <a
                          href="/parent/playlist/${p.id}"
                          class="secondary"
                          style="padding: 0.4rem 0.75rem; border-radius: 0.375rem; border: 1px solid var(--wa-color-surface-border)"
                        >
                          Edit
                        </a>
                        <button
                          class="secondary"
                          type="button"
                          @click=${() => void this.deletePlaylist(p.id)}
                        >
                          Delete
                        </button>
                      </div>
                    </div>
                  `,
                )}
              </div>
            `}
      <hometube-family-playlist-form></hometube-family-playlist-form>
    `;
  }
}

declare global {
  interface HTMLElementTagNameMap {
    'hometube-family-playlist-manager': FamilyPlaylistManager;
  }
}
