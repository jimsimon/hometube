/**
 * <hometube-search-results q="..." type="all|channel|playlist|video">
 *
 * Lit component that fetches `/api/search` and renders mixed-bucket
 * results using the existing `<hometube-channel-card>`,
 * `<hometube-playlist-card>`, and `<hometube-video-card>` components.
 *
 * Behaviour:
 *   - On connect (and whenever the `q` / `type` attributes change) it
 *     re-issues the search.
 *   - Loading + error states render an ARIA live region so screen
 *     readers announce them.
 *   - Pagination via `next_page_token` is supported but the current
 *     backend does not actually paginate; the "Load more" button is
 *     hidden when no token is returned.
 *   - Re-rendering also embeds an `<hometube-search-bar>` at the top
 *     of the page so the user can refine without going back.
 */

import { LitElement, html, css, nothing } from 'lit';
import { customElement, property, state } from 'lit/decorators.js';

import { api, ApiError } from '../services/api.js';

import './search-bar.js';
import './channel-card.js';
import './playlist-card.js';
import './video-card.js';

interface ChannelHit {
  channel_id: string;
  channel_title: string;
  channel_thumbnail_url: string | null;
}

interface PlaylistHit {
  playlist_id: string;
  playlist_title: string;
  playlist_thumbnail_url: string | null;
  source: 'allowlist' | 'own' | 'family';
}

interface VideoHit {
  video_id: string;
  title: string;
  channel_id: string | null;
  channel_title: string | null;
  thumbnail_url: string | null;
}

interface SearchResponse {
  q: string;
  kind: string;
  results: {
    channels: ChannelHit[];
    playlists: PlaylistHit[];
    videos: VideoHit[];
  };
  next_page_token: string | null;
}

@customElement('hometube-search-results')
export class SearchResults extends LitElement {
  @property({ type: String }) q = '';
  @property({ type: String }) type: 'all' | 'channel' | 'playlist' | 'video' =
    'all';

  @state() private channels: ChannelHit[] = [];
  @state() private playlists: PlaylistHit[] = [];
  @state() private videos: VideoHit[] = [];
  @state() private loading = false;
  @state() private error = '';
  @state() private nextPageToken: string | null = null;

  static styles = css`
    :host {
      display: block;
    }
    .bar {
      margin-bottom: 1.5rem;
    }
    h2 {
      font-size: 1.1rem;
      margin: 1.5rem 0 0.75rem;
    }
    .grid {
      display: grid;
      grid-template-columns: repeat(auto-fill, minmax(14rem, 1fr));
      gap: 1rem;
    }
    .empty {
      padding: 2rem 1rem;
      text-align: center;
      color: var(--wa-color-text-quiet);
    }
    .live {
      position: absolute;
      width: 1px;
      height: 1px;
      overflow: hidden;
      clip: rect(0 0 0 0);
      white-space: nowrap;
    }
    .more {
      margin-top: 1.5rem;
      text-align: center;
    }
    .more button {
      padding: 0.5rem 1.25rem;
      border-radius: 0.375rem;
      border: 1px solid var(--wa-color-surface-border, #ccc);
      background: transparent;
      color: var(--wa-color-text-normal);
      font: inherit;
      cursor: pointer;
    }
  `;

  override connectedCallback(): void {
    super.connectedCallback();
    if (this.q) void this.runSearch(false);
  }

  override updated(changed: Map<string, unknown>): void {
    if (
      (changed.has('q') || changed.has('type')) &&
      this.q
    ) {
      void this.runSearch(false);
    }
  }

  private async runSearch(append: boolean): Promise<void> {
    if (!this.q) return;
    this.loading = true;
    this.error = '';
    if (!append) {
      this.channels = [];
      this.playlists = [];
      this.videos = [];
      this.nextPageToken = null;
    }
    try {
      const params = new URLSearchParams();
      params.set('q', this.q);
      if (this.type) params.set('type', this.type);
      if (append && this.nextPageToken) {
        params.set('page_token', this.nextPageToken);
      }
      const res = await api.get<SearchResponse>(
        `/api/search?${params.toString()}`,
      );
      if (append) {
        this.channels = [...this.channels, ...res.results.channels];
        this.playlists = [...this.playlists, ...res.results.playlists];
        this.videos = [...this.videos, ...res.results.videos];
      } else {
        this.channels = res.results.channels;
        this.playlists = res.results.playlists;
        this.videos = res.results.videos;
      }
      this.nextPageToken = res.next_page_token;
    } catch (err) {
      this.error =
        err instanceof ApiError
          ? `Search failed (HTTP ${err.status}).`
          : (err as Error).message;
    } finally {
      this.loading = false;
    }
  }

  private onLoadMore = (): void => {
    void this.runSearch(true);
  };

  override render() {
    const empty =
      !this.loading &&
      !this.error &&
      this.channels.length === 0 &&
      this.playlists.length === 0 &&
      this.videos.length === 0;

    return html`
      <div class="bar">
        <hometube-search-bar
          initial-q=${this.q}
          initial-type=${this.type}
        ></hometube-search-bar>
      </div>

      <div class="live" role="status" aria-live="polite">
        ${this.loading
          ? 'Searching…'
          : this.error
            ? `Error: ${this.error}`
            : `Found ${this.channels.length} channels, ${this.playlists.length} playlists, ${this.videos.length} videos.`}
      </div>

      ${empty
        ? html`<p class="empty">
            No results yet. Try a different word, or ask a parent to add what
            you're looking for.
          </p>`
        : nothing}

      ${this.error
        ? html`<p class="empty" role="alert">${this.error}</p>`
        : nothing}

      ${this.channels.length > 0
        ? html`
            <h2>Channels</h2>
            <div class="grid">
              ${this.channels.map(
                (c) => html`<hometube-channel-card
                  channel-id=${c.channel_id}
                  title=${c.channel_title}
                  .thumbnailUrl=${c.channel_thumbnail_url}
                ></hometube-channel-card>`,
              )}
            </div>
          `
        : nothing}

      ${this.playlists.length > 0
        ? html`
            <h2>Playlists</h2>
            <div class="grid">
              ${this.playlists.map((p) => {
                // Family playlists encode their id as `family:N`; the
                // child playlist deep-link is on the local id only.
                const isLocal = !/^[A-Z]/.test(p.playlist_id);
                if (isLocal) {
                  return html`<hometube-playlist-card
                    playlist-id=${Number(p.playlist_id) || 0}
                    title=${p.playlist_title}
                    .thumbnailUrl=${p.playlist_thumbnail_url}
                    ?is-own=${p.source !== 'allowlist'}
                  ></hometube-playlist-card>`;
                }
                return html`<a
                  href="/child/playlist/${encodeURIComponent(p.playlist_id)}"
                  >${p.playlist_title}</a
                >`;
              })}
            </div>
          `
        : nothing}

      ${this.videos.length > 0
        ? html`
            <h2>Videos</h2>
            <div class="grid">
              ${this.videos.map(
                (v) => html`<hometube-video-card
                  video-id=${v.video_id}
                  title=${v.title}
                  .thumbnailUrl=${v.thumbnail_url}
                  .channelTitle=${v.channel_title}
                ></hometube-video-card>`,
              )}
            </div>
          `
        : nothing}

      ${this.nextPageToken
        ? html`
            <div class="more">
              <button type="button" @click=${this.onLoadMore} ?disabled=${this.loading}>
                ${this.loading ? 'Loading…' : 'Load more'}
              </button>
            </div>
          `
        : nothing}
    `;
  }
}

declare global {
  interface HTMLElementTagNameMap {
    'hometube-search-results': SearchResults;
  }
}
