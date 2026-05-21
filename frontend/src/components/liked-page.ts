/**
 * <hometube-liked-page>
 *
 * The body of `/child/liked`. Fetches `GET /api/likes` and renders a
 * grid of `<hometube-video-card>` for every like whose target video is
 * still reachable through the child's allowlist (`visible: true`).
 * Likes pointing at videos the parent has since removed from the
 * allowlist (`visible: false`) are filtered out client-side so the
 * grid never surfaces a card that would 403 on click. When that
 * filtering hides any rows, a small "hidden by your parent" note is
 * rendered so the child isn't left wondering why the count changed.
 *
 * The component is disposable-safe: a per-load token is captured before
 * each fetch and checked after the await so a stale in-flight response
 * can't overwrite state on a remounted (or already-unmounted) element.
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
  @state() private hiddenCount = 0;
  @state() private loading = false;
  @state() private error = "";

  /** Monotonic load token. Bumped on each `load()` and on disconnect so
   *  any in-flight response from an earlier load is discarded. */
  private loadToken = 0;

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
    .hidden-note {
      color: var(--wa-color-text-quiet);
      font-size: 0.875rem;
      padding: 0.5rem 0 0;
    }
  `;

  override connectedCallback(): void {
    super.connectedCallback();
    void this.load();
  }

  override disconnectedCallback(): void {
    // Invalidate any in-flight load so its `.then` is a no-op.
    this.loadToken++;
    super.disconnectedCallback();
  }

  private async load(): Promise<void> {
    const token = ++this.loadToken;
    this.loading = true;
    this.error = "";
    try {
      const rows = await api.get<LikeRow[]>("/api/likes");
      if (token !== this.loadToken) return;
      const visible = rows.filter((r) => r.visible);
      this.items = visible;
      this.hiddenCount = rows.length - visible.length;
    } catch (err) {
      if (token !== this.loadToken) return;
      this.error = (err as Error).message;
    } finally {
      if (token === this.loadToken) {
        this.loading = false;
      }
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
      if (this.hiddenCount > 0) {
        return html`<p class="empty">
          The ${this.hiddenCount === 1 ? "video" : "videos"} you liked
          ${this.hiddenCount === 1 ? "isn't" : "aren't"} available right now. Ask a grown-up if
          you'd like to watch ${this.hiddenCount === 1 ? "it" : "them"} again.
        </p>`;
      }
      return html`<p class="empty">
        You haven't liked any videos yet. Tap the like button on a video to save it here.
      </p>`;
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
              .channelId=${v.channel_id}
              .channelTitle=${v.channel_title}
            ></hometube-video-card>
          `,
        )}
      </div>
      ${this.hiddenCount > 0
        ? html`<p class="hidden-note">
            ${this.hiddenCount} more liked
            ${this.hiddenCount === 1 ? "video isn't" : "videos aren't"} available right now.
          </p>`
        : ""}
    `;
  }
}

declare global {
  interface HTMLElementTagNameMap {
    "hometube-liked-page": LikedPage;
  }
}
