/**
 * <hometube-liked-page>
 *
 * The body of `/child/liked`. Fetches `GET /api/likes` and renders a
 * grid of `<hometube-video-card>` for every like whose target video is
 * still reachable through the child's allowlist (`visible: true`).
 * Likes pointing at videos the parent has since removed from the
 * allowlist (`visible: false`) are filtered out client-side so the
 * grid never surfaces a card that would 403 on click.
 */

import { LitElement, html, css } from "lit";
import { customElement, state } from "lit/decorators.js";

import { api } from "../services/api.js";
import type { LikeRow } from "../types/index.js";

import "./video-card.js";
import "./loading-spinner.js";
import "./error-banner.js";

@customElement("hometube-liked-page")
export class LikedPage extends LitElement {
  @state() private items: LikeRow[] = [];
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
  }

  private async load(): Promise<void> {
    this.loading = true;
    this.error = "";
    try {
      const rows = await api.get<LikeRow[]>("/api/likes");
      this.items = rows.filter((r) => r.visible);
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
      return html`<p class="empty">You haven't liked any videos yet. Tap the like button on a video to save it here.</p>`;
    }
    return html`
      <div class="grid" role="list">
        ${this.items.map(
          (v) => html`
            <hometube-video-card
              role="listitem"
              video-id=${v.video_id}
              title=${v.video_title ?? ""}
              .thumbnailUrl=${v.video_thumbnail_url}
            ></hometube-video-card>
          `,
        )}
      </div>
    `;
  }
}

declare global {
  interface HTMLElementTagNameMap {
    "hometube-liked-page": LikedPage;
  }
}
