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

import { LitElement, html, css, nothing } from "lit";
import { customElement, property, state } from "lit/decorators.js";

import { api, ApiError } from "../services/api.js";
import type {
  ChildSearchChannelHit,
  ChildSearchPlaylistHit,
  ChildSearchPlaylistSource,
  ChildSearchResponse,
  ChildSearchVideoHit,
} from "../types/index.js";

import "./search-bar.js";
import "./channel-card.js";
import "./playlist-card.js";
import "./video-card.js";
import "./loading-spinner.js";

const PLAYLIST_BADGE_LABELS: Record<ChildSearchPlaylistSource, string> = {
  allowlist: "Allowed",
  own: "My playlist",
  family: "Family",
};

@customElement("hometube-search-results")
export class SearchResults extends LitElement {
  @property({ type: String }) q = "";
  @property({ type: String }) type: "all" | "channel" | "playlist" | "video" = "all";

  @state() private channels: ChildSearchChannelHit[] = [];
  @state() private playlists: ChildSearchPlaylistHit[] = [];
  @state() private videos: ChildSearchVideoHit[] = [];
  @state() private loading = false;
  @state() private error = "";
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
      grid-template-columns: repeat(auto-fill, minmax(min(15rem, 100%), 1fr));
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
    .playlist-result {
      position: relative;
      display: block;
    }
    .badge {
      position: absolute;
      top: 0.5rem;
      left: 0.5rem;
      padding: 0.125rem 0.5rem;
      font-size: 0.75rem;
      font-weight: 600;
      border-radius: 999px;
      background: var(--wa-color-surface-default, #fff);
      color: var(--wa-color-text-normal);
      border: 1px solid var(--wa-color-surface-border, #ccc);
      pointer-events: none;
      z-index: 1;
    }
    .badge.family {
      background: var(--wa-color-brand-quiet, #eef);
      border-color: var(--wa-color-brand-on-quiet, #88a);
    }
    .badge.own {
      background: var(--wa-color-success-quiet, #efe);
      border-color: var(--wa-color-success-on-quiet, #8a8);
    }
    .external-link {
      display: block;
      padding: 0.75rem;
      border: 1px solid var(--wa-color-surface-border, #ccc);
      border-radius: 0.5rem;
      color: var(--wa-color-text-normal);
      text-decoration: none;
    }
    .external-link:hover,
    .external-link:focus {
      background: var(--wa-color-surface-quiet, #f5f5f5);
    }
  `;

  override connectedCallback(): void {
    super.connectedCallback();
    if (this.q) void this.runSearch(false);
  }

  override updated(changed: Map<string, unknown>): void {
    if ((changed.has("q") || changed.has("type")) && this.q) {
      void this.runSearch(false);
    }
  }

  private async runSearch(append: boolean): Promise<void> {
    if (!this.q) return;
    this.loading = true;
    this.error = "";
    if (!append) {
      this.channels = [];
      this.playlists = [];
      this.videos = [];
      this.nextPageToken = null;
    }
    try {
      const params = new URLSearchParams();
      params.set("q", this.q);
      if (this.type) params.set("type", this.type);
      if (append && this.nextPageToken) {
        params.set("page_token", this.nextPageToken);
      }
      const res = await api.get<ChildSearchResponse>(`/api/search?${params.toString()}`);
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
        err instanceof ApiError ? `Search failed (HTTP ${err.status}).` : (err as Error).message;
    } finally {
      this.loading = false;
    }
  }

  private onLoadMore = (): void => {
    void this.runSearch(true);
  };

  override render() {
    const hasResults =
      this.channels.length > 0 || this.playlists.length > 0 || this.videos.length > 0;
    const empty = !this.loading && !this.error && !hasResults;

    return html`
      <div class="bar">
        <hometube-search-bar initial-q=${this.q} initial-type=${this.type}></hometube-search-bar>
      </div>

      <div class="live" role="status" aria-live="polite">
        ${this.loading
          ? "Searching…"
          : this.error
            ? `Error: ${this.error}`
            : `Found ${this.channels.length} channels, ${this.playlists.length} playlists, ${this.videos.length} videos.`}
      </div>

      ${this.loading && !hasResults
        ? html`<hometube-loading-spinner label="Searching…"></hometube-loading-spinner>`
        : nothing}
      ${empty
        ? html`<p class="empty">
            No results yet. Try a different word, or ask a parent to add what you're looking for.
          </p>`
        : nothing}
      ${this.error ? html`<p class="empty" role="alert">${this.error}</p>` : nothing}
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
                const badge = html`<span
                  class="badge ${p.source}"
                  aria-label="Playlist source: ${PLAYLIST_BADGE_LABELS[p.source]}"
                  >${PLAYLIST_BADGE_LABELS[p.source]}</span
                >`;
                // Family playlists encode their id as `family:N`; the
                // child playlist deep-link is on the local id only.
                const isLocal = p.source !== "family" && !/^[A-Z]/.test(p.playlist_id);
                if (isLocal) {
                  return html`<div class="playlist-result">
                    ${badge}
                    <hometube-playlist-card
                      playlist-id=${Number(p.playlist_id) || 0}
                      title=${p.playlist_title}
                      .thumbnailUrl=${p.playlist_thumbnail_url}
                      ?is-own=${p.source === "own"}
                    ></hometube-playlist-card>
                  </div>`;
                }
                return html`<div class="playlist-result">
                  ${badge}
                  <hometube-playlist-card
                    playlist-id=${p.playlist_id}
                    title=${p.playlist_title}
                    .thumbnailUrl=${p.playlist_thumbnail_url}
                  ></hometube-playlist-card>
                </div>`;
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
                ${this.loading ? "Loading…" : "Load more"}
              </button>
            </div>
          `
        : nothing}
    `;
  }
}

declare global {
  interface HTMLElementTagNameMap {
    "hometube-search-results": SearchResults;
  }
}
