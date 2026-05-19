/**
 * <hometube-channel-detail channel-id="...">
 *
 * Page-level component for /child/channel/:id. Loads channel info from
 * /api/channels/:id and a paginated list of videos from
 * /api/channels/:id/videos. Renders a header with the channel name,
 * subscribe button, and video grid.
 */

import { LitElement, html, css } from "lit";
import { customElement, property, state } from "lit/decorators.js";

import { ApiError, api } from "../services/api.js";
import { pickThumbnail } from "../types/index.js";
import type { ChannelInfo, ChannelVideosPage } from "../types/index.js";

import "./subscribe-button.js";
import "./video-card.js";
import "./error-banner.js";
import "./loading-spinner.js";

@customElement("hometube-channel-detail")
export class ChannelDetail extends LitElement {
  @property({ type: String, attribute: "channel-id" })
  channelId = "";

  @state() private info: ChannelInfo | null = null;
  @state() private videos: ChannelVideosPage["items"] = [];
  @state() private nextPageToken: string | null = null;
  @state() private sort: "latest" | "most_viewed" = "latest";
  @state() private loading = false;
  @state() private error = "";

  static styles = css`
    :host {
      display: block;
    }
    header {
      display: flex;
      gap: 1rem;
      align-items: center;
      padding: 1rem;
      border-bottom: 1px solid var(--wa-color-surface-border);
      flex-wrap: wrap;
    }
    img.avatar {
      width: 4rem;
      height: 4rem;
      border-radius: 50%;
      object-fit: cover;
      background: var(--wa-color-surface-border);
    }
    .meta {
      flex: 1;
      min-width: 0;
    }
    .meta h1 {
      margin: 0;
      font-size: 1.4rem;
    }
    .meta .stats {
      color: var(--wa-color-text-quiet);
      font-size: 0.9rem;
    }
    .controls {
      display: flex;
      gap: 0.75rem;
      padding: 0.75rem 1rem;
      align-items: center;
    }
    select {
      padding: 0.4rem 0.6rem;
      border: 1px solid var(--wa-color-surface-border, #ccc);
      border-radius: 0.375rem;
      background: var(--wa-color-surface-default);
      color: var(--wa-color-text-normal);
      font: inherit;
    }
    .grid {
      display: grid;
      gap: 1rem;
      grid-template-columns: repeat(auto-fill, minmax(min(15rem, 100%), 1fr));
      padding: 1rem;
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
    .more {
      display: flex;
      justify-content: center;
      padding: 1rem;
    }
    button {
      padding: 0.5rem 1rem;
      border-radius: 0.375rem;
      border: 1px solid var(--wa-color-surface-border);
      background: transparent;
      color: var(--wa-color-text-normal);
      font: inherit;
      cursor: pointer;
    }
  `;

  override connectedCallback(): void {
    super.connectedCallback();
    void this.load();
    this.addEventListener("video-hidden", this.onVideoHidden as EventListener);
  }

  override disconnectedCallback(): void {
    this.removeEventListener("video-hidden", this.onVideoHidden as EventListener);
    super.disconnectedCallback();
  }

  private onVideoHidden = (e: CustomEvent<{ videoId: string }>) => {
    const videoId = e.detail?.videoId;
    if (!videoId) return;
    this.videos = this.videos.filter((v) => v.video_id !== videoId);
  };

  private async load(): Promise<void> {
    this.loading = true;
    this.error = "";
    this.videos = [];
    this.nextPageToken = null;
    try {
      const [info, page] = await Promise.all([
        api.get<ChannelInfo>(`/api/channels/${encodeURIComponent(this.channelId)}`),
        api.get<ChannelVideosPage>(
          `/api/channels/${encodeURIComponent(this.channelId)}/videos?sort=${this.sort}`,
        ),
      ]);
      this.info = info;
      this.videos = page.items;
      this.nextPageToken = page.next_page_token;
    } catch (err) {
      this.error = err instanceof ApiError ? String(err.body) : (err as Error).message;
    } finally {
      this.loading = false;
    }
  }

  private async loadMore(): Promise<void> {
    if (!this.nextPageToken) return;
    try {
      const page = await api.get<ChannelVideosPage>(
        `/api/channels/${encodeURIComponent(this.channelId)}/videos?sort=${this.sort}&page_token=${encodeURIComponent(this.nextPageToken)}`,
      );
      this.videos = [...this.videos, ...page.items];
      this.nextPageToken = page.next_page_token;
    } catch (err) {
      this.error = (err as Error).message;
    }
  }

  private onSortChange = (e: Event): void => {
    this.sort = (e.target as HTMLSelectElement).value as "latest" | "most_viewed";
    void this.load();
  };

  private avatar(): string | null {
    if (!this.info) return null;
    return pickThumbnail(this.info.thumbnails);
  }

  override render() {
    if (this.error) {
      return html`<hometube-error-banner .message=${this.error}></hometube-error-banner>`;
    }
    return html`
      <header>
        ${this.avatar()
          ? html`<img class="avatar" src=${this.avatar()!} alt="" />`
          : html`<div class="avatar" aria-hidden="true"></div>`}
        <div class="meta">
          <h1>${this.info?.title ?? "Channel"}</h1>
          ${this.info?.subscriber_count != null
            ? html`<div class="stats">
                ${this.info.subscriber_count.toLocaleString()} subscribers
              </div>`
            : null}
        </div>
        <hometube-subscribe-button channel-id=${this.channelId}></hometube-subscribe-button>
      </header>

      <div class="controls">
        <label for="channel-sort">Sort by</label>
        <select id="channel-sort" .value=${this.sort} @change=${this.onSortChange}>
          <option value="latest">Latest</option>
          <option value="most_viewed">Most viewed</option>
        </select>
      </div>

      ${this.loading
        ? html`<hometube-loading-spinner label="Loading videos…"></hometube-loading-spinner>`
        : this.videos.length === 0
          ? html`<p class="empty">No videos available.</p>`
          : html`
              <div class="grid" role="list">
                ${this.videos.map(
                  (v) => html`
                    <hometube-video-card
                      role="listitem"
                      video-id=${v.video_id}
                      title=${v.title}
                      thumbnail-url=${pickThumbnail(v.thumbnails) ?? ""}
                      channel-title=${v.channel_title ?? ""}
                    ></hometube-video-card>
                  `,
                )}
              </div>
              ${this.nextPageToken
                ? html`<div class="more">
                    <button type="button" @click=${() => void this.loadMore()}>Load more</button>
                  </div>`
                : null}
            `}
    `;
  }
}

declare global {
  interface HTMLElementTagNameMap {
    "hometube-channel-detail": ChannelDetail;
  }
}
