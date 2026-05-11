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

@customElement("hometube-up-next")
export class UpNext extends LitElement {
  @property({ type: String, attribute: "video-id" })
  videoId = "";

  @property({ type: String })
  from = "";

  @state() private items: UpNextItem[] = [];
  @state() private settings: ChildSettings | null = null;
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
    const key = "hometube-autoplay-count";
    const current = Number(sessionStorage.getItem(key) ?? "0");
    sessionStorage.setItem(key, String(current + 1));
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
    window.location.href = url.toString();
  };

  override render() {
    if (this.error) {
      return html`<p class="empty" role="alert">${this.error}</p>`;
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
                    <a
                      href="/child/video/${encodeURIComponent(it.video_id)}${this.from
                        ? `?from=${encodeURIComponent(this.from)}`
                        : ""}"
                    >
                      ${it.thumbnail_url
                        ? html`<img src=${it.thumbnail_url} alt="" />`
                        : html`<div class="placeholder"></div>`}
                      <div>
                        <div class="row-title">${it.title}</div>
                        ${it.channel_title
                          ? html`<div class="row-channel">${it.channel_title}</div>`
                          : null}
                      </div>
                    </a>
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
