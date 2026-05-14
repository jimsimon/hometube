/**
 * <hometube-video-row feed="continue-watching|new-videos" heading="...">
 *
 * Horizontal scrolling row of `<hometube-video-card>`s. Fetches the
 * appropriate feed endpoint on connect and renders a heading above the
 * row.
 */

import { LitElement, html, css, nothing } from "lit";
import { customElement, property, state } from "lit/decorators.js";

import { api } from "../services/api.js";
import type { ContinueWatchingItem, NewVideoItem } from "../types/index.js";

import "./video-card.js";
import "./loading-spinner.js";
import "./error-banner.js";

type Feed = "continue-watching" | "new-videos";

interface Card {
  videoId: string;
  title: string;
  thumbnailUrl: string | null;
  channelTitle: string | null;
  durationSeconds: number | null;
  progress: number;
}

@customElement("hometube-video-row")
export class VideoRow extends LitElement {
  @property({ type: String })
  feed: Feed = "new-videos";

  @property({ type: String })
  heading = "";

  @state() private cards: Card[] = [];
  @state() private loading = false;
  @state() private error = "";

  static styles = css`
    :host {
      display: block;
      margin-block: 1.5rem;
    }
    h2 {
      margin: 0 0 0.5rem;
      font-size: 1.25rem;
    }
    .scroller {
      display: flex;
      gap: 1rem;
      overflow-x: auto;
      scroll-snap-type: x mandatory;
      padding-bottom: 0.5rem;
    }
    .scroller > * {
      flex: 0 0 16rem;
      scroll-snap-align: start;
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
  }

  private async load(): Promise<void> {
    this.loading = true;
    this.error = "";
    try {
      if (this.feed === "continue-watching") {
        const items = await api.get<ContinueWatchingItem[]>("/api/feed/continue-watching");
        this.cards = items.map((it) => ({
          videoId: it.video_id,
          title: it.video_title,
          thumbnailUrl: it.video_thumbnail_url,
          channelTitle: it.channel_title,
          durationSeconds: it.duration_seconds,
          progress:
            it.duration_seconds && it.duration_seconds > 0
              ? Math.min(1, it.progress_seconds / it.duration_seconds)
              : 0,
        }));
      } else {
        const items = await api.get<NewVideoItem[]>("/api/feed/new-videos");
        this.cards = items.map((it) => ({
          videoId: it.video_id,
          title: it.title,
          thumbnailUrl: it.thumbnail_url,
          channelTitle: it.channel_title,
          durationSeconds: null,
          progress: 0,
        }));
      }
    } catch (err) {
      this.error = (err as Error).message;
    } finally {
      this.loading = false;
    }
  }

  override render() {
    return html`
      ${this.heading ? html`<h2>${this.heading}</h2>` : nothing}
      ${this.loading
        ? html`<hometube-loading-spinner></hometube-loading-spinner>`
        : this.error
          ? html`<hometube-error-banner .message=${this.error}></hometube-error-banner>`
          : this.cards.length === 0
            ? html`<p class="empty">Nothing here yet.</p>`
            : html`
                <div class="scroller" role="list">
                  ${this.cards.map(
                    (c) => html`
                      <hometube-video-card
                        role="listitem"
                        video-id=${c.videoId}
                        title=${c.title}
                        .thumbnailUrl=${c.thumbnailUrl}
                        .channelTitle=${c.channelTitle}
                        .duration=${c.durationSeconds}
                        progress=${c.progress}
                      ></hometube-video-card>
                    `,
                  )}
                </div>
              `}
    `;
  }
}

declare global {
  interface HTMLElementTagNameMap {
    "hometube-video-row": VideoRow;
  }
}
