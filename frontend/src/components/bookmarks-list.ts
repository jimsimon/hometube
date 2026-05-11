/**
 * <hometube-bookmarks-list>
 *
 * Page-level component for /child/bookmarks. Loads /api/bookmarks and
 * groups results by video so a child can quickly find a previously
 * bookmarked moment. Each bookmark links to the video page with a
 * `?t=<seconds>` query so the player can seek on load.
 */

import { LitElement, html, css, nothing } from "lit";
import { customElement, state } from "lit/decorators.js";

import { ApiError, api } from "../services/api.js";
import type { Bookmark } from "../types/index.js";

import "./loading-spinner.js";
import "./error-banner.js";

interface VideoGroup {
  videoId: string;
  videoTitle: string | null;
  bookmarks: Bookmark[];
}

@customElement("hometube-bookmarks-list")
export class BookmarksList extends LitElement {
  @state() private bookmarks: Bookmark[] = [];
  @state() private loading = false;
  @state() private error = "";

  static styles = css`
    :host {
      display: block;
    }
    .group {
      padding: 0.75rem;
      border: 1px solid var(--wa-color-surface-border);
      border-radius: 0.5rem;
      background: var(--wa-color-surface-default);
      margin-block: 0.75rem;
    }
    .group-title {
      font-weight: 600;
      margin: 0 0 0.5rem;
    }
    .bookmarks {
      display: flex;
      flex-wrap: wrap;
      gap: 0.5rem;
      list-style: none;
      padding: 0;
      margin: 0;
    }
    .bookmarks a {
      display: inline-flex;
      align-items: center;
      gap: 0.5rem;
      padding: 0.4rem 0.75rem;
      border-radius: 999px;
      border: 1px solid var(--wa-color-surface-border);
      background: var(--wa-color-surface-raised);
      color: var(--wa-color-text-normal);
      text-decoration: none;
      font-size: 0.9rem;
    }
    .bookmarks a:focus-visible {
      outline: 2px solid var(--wa-color-brand-fill, #2563eb);
      outline-offset: 2px;
    }
    .timestamp {
      font-variant-numeric: tabular-nums;
      color: var(--wa-color-text-quiet);
    }
    .empty {
      color: var(--wa-color-text-quiet);
      font-style: italic;
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
      this.bookmarks = await api.get<Bookmark[]>("/api/bookmarks?limit=200");
    } catch (err) {
      this.error = err instanceof ApiError ? String(err.body) : (err as Error).message;
    } finally {
      this.loading = false;
    }
  }

  private groupByVideo(): VideoGroup[] {
    const map = new Map<string, VideoGroup>();
    for (const b of this.bookmarks) {
      let group = map.get(b.video_id);
      if (!group) {
        group = {
          videoId: b.video_id,
          videoTitle: b.video_title,
          bookmarks: [],
        };
        map.set(b.video_id, group);
      }
      group.bookmarks.push(b);
    }
    for (const group of map.values()) {
      group.bookmarks.sort((a, b) => a.timestamp_seconds - b.timestamp_seconds);
    }
    return [...map.values()];
  }

  private formatTime(seconds: number): string {
    const m = Math.floor(seconds / 60);
    const s = seconds % 60;
    return `${m}:${String(s).padStart(2, "0")}`;
  }

  override render() {
    if (this.loading) {
      return html`<hometube-loading-spinner label="Loading bookmarks…"></hometube-loading-spinner>`;
    }
    if (this.error) {
      return html`<hometube-error-banner message=${this.error}></hometube-error-banner>`;
    }
    const groups = this.groupByVideo();
    if (groups.length === 0) {
      return html`<p class="empty">
        No bookmarks yet. Tap the bookmark button while watching a video to save a moment.
      </p>`;
    }
    return html`
      ${groups.map(
        (g) => html`
          <section class="group" aria-label=${g.videoTitle ?? g.videoId}>
            <h2 class="group-title">${g.videoTitle ?? g.videoId}</h2>
            <ul class="bookmarks">
              ${g.bookmarks.map(
                (b) => html`
                  <li>
                    <a
                      href="/child/video/${encodeURIComponent(g.videoId)}?t=${b.timestamp_seconds}"
                      aria-label=${`Jump to ${this.formatTime(b.timestamp_seconds)}${b.label ? ` — ${b.label}` : ""}`}
                    >
                      <span class="timestamp">${this.formatTime(b.timestamp_seconds)}</span>
                      ${b.label ? html`<span>${b.label}</span>` : nothing}
                    </a>
                  </li>
                `,
              )}
            </ul>
          </section>
        `,
      )}
    `;
  }
}

declare global {
  interface HTMLElementTagNameMap {
    "hometube-bookmarks-list": BookmarksList;
  }
}
