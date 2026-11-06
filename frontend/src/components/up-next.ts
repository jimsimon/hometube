/**
 * <hometube-up-next video-id="..." from="...">
 *
 * Sidebar list of videos to play after the current one. Reads
 * /api/feed/up-next?from=...&current_video=...&limit=10. Listens for
 * the player's `ended` event (bubbled up through the DOM as
 * `hometube:video-ended`) to optionally auto-advance.
 */

import { LitElement, html, css } from "lit";
import { customElement, property, state } from "lit/decorators.js";

import { api } from "../services/api.js";
import type { ChildSettings, UpNextItem } from "../types/index.js";

import "./loading-spinner.js";
import "./error-banner.js";
import "./content-card.js";

/** Consecutive auto-advances in this tab. Shared with the player. */
const AUTOPLAY_COUNT_KEY = "hometube-autoplay-count";
/**
 * Video ID the player should treat as auto-started on its next page.
 * Set just before navigating; consumed by the player's first `play`.
 */
const AUTOPLAY_PENDING_KEY = "hometube-autoplay-pending";

@customElement("hometube-up-next")
export class UpNext extends LitElement {
  @property({ type: String, attribute: "video-id" })
  videoId = "";

  @property({ type: String })
  from = "";

  @state() private items: UpNextItem[] = [];
  @state() private settings: ChildSettings | null = null;
  @state() private loading = false;
  @state() private error = "";

  static styles = css`
    :host {
      display: block;
    }
    h2 {
      margin: 0 0 0.5rem;
      font-size: 1.1rem;
    }
    ul {
      list-style: none;
      padding: 0;
      margin: 0;
      display: grid;
      gap: 0.5rem;
    }
    li {
      border: 1px solid var(--wa-color-surface-border);
      border-radius: 0.375rem;
      background: var(--wa-color-surface-default);
    }
    a {
      display: grid;
      grid-template-columns: auto 1fr;
      gap: 0.6rem;
      padding: 0.5rem;
      text-decoration: none;
      color: inherit;
      align-items: center;
    }
    a:hover,
    a:focus-visible {
      background: var(--wa-color-surface-raised);
      outline: none;
    }
    img {
      width: 6rem;
      height: 3.5rem;
      object-fit: cover;
      border-radius: 0.25rem;
      background: var(--wa-color-surface-border);
    }
    .row-title {
      font-weight: 600;
      font-size: 0.95rem;
      line-height: 1.3;
      overflow: hidden;
      display: -webkit-box;
      -webkit-line-clamp: 2;
      -webkit-box-orient: vertical;
    }
    .row-channel {
      font-size: 0.85rem;
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
    document.addEventListener("hometube:video-ended", this.onVideoEnded as EventListener);
  }

  override disconnectedCallback(): void {
    super.disconnectedCallback();
    document.removeEventListener("hometube:video-ended", this.onVideoEnded as EventListener);
  }

  override updated(changed: Map<string, unknown>): void {
    if (changed.has("videoId") || changed.has("from")) {
      void this.load();
    }
  }

  private async load(): Promise<void> {
    if (!this.videoId) return;
    this.loading = true;
    this.error = "";
    const params = new URLSearchParams();
    if (this.from) params.set("from", this.from);
    params.set("current_video", this.videoId);
    params.set("limit", "10");
    try {
      const [items, settings] = await Promise.all([
        api.get<UpNextItem[]>(`/api/feed/up-next?${params.toString()}`),
        api.get<ChildSettings>("/api/children/me/settings").catch(() => null),
      ]);
      this.items = items;
      this.settings = settings;
    } catch (err) {
      this.error = (err as Error).message;
    } finally {
      this.loading = false;
    }
  }

  /** Handle the player's `ended` event: auto-advance if allowed. */
  private onVideoEnded = (): void => {
    if (!this.settings?.autoplay_enabled) return;
    if (this.items.length === 0) return;
    const next = this.items[0];
    if (!next) return;
    const url = new URL(window.location.href);
    url.pathname = `/child/video/${encodeURIComponent(next.video_id)}`;
    if (this.from) url.searchParams.set("from", this.from);
    else url.searchParams.delete("from");
    // Track auto-advance count via sessionStorage so the player can
    // refuse it if the autoplay_max_consecutive cap is reached.
    let current = 0;
    try {
      current = Number(sessionStorage.getItem(AUTOPLAY_COUNT_KEY) ?? "0") || 0;
    } catch {
      current = 0;
    }
    if (
      this.settings.autoplay_max_consecutive != null &&
      current + 1 > this.settings.autoplay_max_consecutive
    ) {
      // Cap reached — surface a continue prompt instead of navigating.
      this.dispatchEvent(
        new CustomEvent("hometube:autoplay-cap-reached", {
          bubbles: true,
          composed: true,
          detail: { nextVideoId: next.video_id },
        }),
      );
      return;
    }
    try {
      sessionStorage.setItem(AUTOPLAY_COUNT_KEY, String(current + 1));
      // Tell the player on the next page that its first `play` is this
      // auto-advance, not a tap from the child. Without the marker the
      // player treated every page's first play as manual and reset the
      // counter to 0, so the "pause after N videos in a row" cap never
      // fired.
      sessionStorage.setItem(AUTOPLAY_PENDING_KEY, next.video_id);
    } catch {
      // sessionStorage unavailable — autoplay still works, the cap
      // just can't be tracked.
    }
    window.location.href = url.toString();
  };

  override render() {
    if (this.loading && this.items.length === 0) {
      return html`
        <h2>Up next</h2>
        <hometube-loading-spinner></hometube-loading-spinner>
      `;
    }
    if (this.error) {
      return html`
        <h2>Up next</h2>
        <hometube-error-banner .message=${this.error}></hometube-error-banner>
      `;
    }
    return html`
      <h2>Up next</h2>
      ${this.items.length === 0
        ? html`<p class="empty">Nothing queued.</p>`
        : html`
            <ul>
              ${this.items.map(
                (it) => html`
                  <li>
                    <hometube-content-card
                      variant="compact"
                      title=${it.title}
                      .thumbnailUrl=${it.thumbnail_url}
                      .channelTitle=${it.channel_title}
                      href="/child/video/${encodeURIComponent(it.video_id)}${this.from
                        ? `?from=${encodeURIComponent(this.from)}`
                        : ""}"
                    ></hometube-content-card>
                  </li>
                `,
              )}
            </ul>
          `}
    `;
  }
}

declare global {
  interface HTMLElementTagNameMap {
    "hometube-up-next": UpNext;
  }
}
