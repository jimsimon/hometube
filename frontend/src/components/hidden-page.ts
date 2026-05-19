/**
 * <hometube-hidden-page>
 *
 * The body of `/child/hidden`. Fetches `GET /api/hidden` and renders
 * a grid of `<hometube-video-card mode="hidden">` with an Unhide
 * button on each. Listens for the `video-unhidden` event the card
 * dispatches after a successful `DELETE /api/hidden/:videoId` and
 * removes that card from the local list.
 */

import { LitElement, html, css } from "lit";
import { customElement, state } from "lit/decorators.js";

import { api } from "../services/api.js";
import type { HiddenVideo } from "../types/index.js";

import "./video-card.js";
import "./loading-spinner.js";
import "./error-banner.js";

@customElement("hometube-hidden-page")
export class HiddenPage extends LitElement {
  @state() private items: HiddenVideo[] = [];
  @state() private loading = false;
  @state() private error = "";

  static styles = css`
    :host {
      display: block;
    }
    .grid {
      display: grid;
      gap: 1rem;
      grid-template-columns: repeat(auto-fill, minmax(min(15rem, 100%), 1fr));
      padding: 1rem 0;
    }
    .empty {
      color: var(--wa-color-text-quiet);
      font-style: italic;
      padding: 1rem 0;
    }
  `;

  override connectedCallback(): void {
    super.connectedCallback();
    void this.load();
    this.addEventListener("video-unhidden", this.onVideoUnhidden as EventListener);
  }

  override disconnectedCallback(): void {
    this.removeEventListener("video-unhidden", this.onVideoUnhidden as EventListener);
    super.disconnectedCallback();
  }

  private onVideoUnhidden = (e: CustomEvent<{ videoId: string }>) => {
    const videoId = e.detail?.videoId;
    if (!videoId) return;
    this.items = this.items.filter((v) => v.video_id !== videoId);
  };

  private async load(): Promise<void> {
    this.loading = true;
    this.error = "";
    try {
      this.items = await api.get<HiddenVideo[]>("/api/hidden");
    } catch (err) {
      this.error = (err as Error).message;
    } finally {
      this.loading = false;
    }
  }

  override render() {
    if (this.loading) {
      return html`<hometube-loading-spinner></hometube-loading-spinner>`;
    }
    if (this.error) {
      return html`<hometube-error-banner .message=${this.error}></hometube-error-banner>`;
    }
    if (this.items.length === 0) {
      return html`<p class="empty">You haven't hidden any videos yet.</p>`;
    }
    return html`
      <div class="grid" role="list">
        ${this.items.map(
          (v) => html`
            <hometube-video-card
              role="listitem"
              mode="hidden"
              video-id=${v.video_id}
              title=${v.video_title ?? ""}
              .thumbnailUrl=${v.video_thumbnail_url}
              .channelId=${v.channel_id}
              .channelTitle=${v.channel_title}
              .duration=${v.duration_seconds}
            ></hometube-video-card>
          `,
        )}
      </div>
    `;
  }
}

declare global {
  interface HTMLElementTagNameMap {
    "hometube-hidden-page": HiddenPage;
  }
}
