/**
 * <hometube-video-card>
 *
 * A single thumbnail tile used by `<hometube-video-row>` and search
 * results. Renders a thumbnail, title, channel name, and an optional
 * duration badge.
 *
 * Two modes:
 * - default: the card links to `/child/video/:videoId` and exposes a
 *   kebab menu with a "Hide this video" action. On hide, the card
 *   `POST`s to `/api/hidden` and dispatches a bubbling `video-hidden`
 *   `CustomEvent` so parent listings can remove it optimistically.
 * - `mode="hidden"`: used on the `/child/hidden` page. The card is not
 *   a link, and the action button becomes "Unhide" which `DELETE`s
 *   `/api/hidden/:videoId` and dispatches `video-unhidden`.
 */

import { LitElement, html, css } from "lit";
import { customElement, property, state } from "lit/decorators.js";
import { api, ApiError } from "../services/api.js";
import { normalizeThumbnailUrl } from "../types/index.js";

@customElement("hometube-video-card")
export class VideoCard extends LitElement {
  @property({ type: String, attribute: "video-id" })
  videoId = "";

  @property({ type: String })
  title = "";

  @property({ type: String, attribute: "thumbnail-url" })
  thumbnailUrl: string | null = null;

  @property({ type: String, attribute: "channel-id" })
  channelId: string | null = null;

  @property({ type: String, attribute: "channel-title" })
  channelTitle: string | null = null;

  /** Duration in seconds. Renders as a "M:SS" / "H:MM:SS" badge. */
  @property({ type: Number })
  duration: number | null = null;

  /** 0..1 progress indicator (continue-watching). */
  @property({ type: Number })
  progress = 0;

  /** "default" (link + Hide action) or "hidden" (no link, Unhide action). */
  @property({ type: String })
  mode: "default" | "hidden" = "default";

  @state()
  private menuOpen = false;

  @state()
  private busy = false;

  @state()
  private actionError = "";

  static styles = css`
    :host {
      display: block;
    }
    .card {
      position: relative;
      display: flex;
      flex-direction: column;
      gap: 0.5rem;
      border-radius: 0.5rem;
      padding: 0.5rem;
    }
    a.card-link,
    .card-body {
      display: flex;
      flex-direction: column;
      gap: 0.5rem;
      text-decoration: none;
      color: inherit;
      border-radius: 0.375rem;
    }
    a.card-link:hover,
    a.card-link:focus-visible {
      background: var(--wa-color-surface-raised);
      outline: none;
    }
    .thumb {
      position: relative;
      aspect-ratio: 16 / 9;
      border-radius: 0.375rem;
      overflow: hidden;
      background: var(--wa-color-surface-border);
    }
    .thumb img {
      width: 100%;
      height: 100%;
      object-fit: cover;
      display: block;
    }
    .duration {
      position: absolute;
      bottom: 0.25rem;
      right: 0.25rem;
      padding: 0.125rem 0.375rem;
      border-radius: 0.25rem;
      background: rgba(0, 0, 0, 0.75);
      color: white;
      font-size: 0.75rem;
    }
    .progress {
      position: absolute;
      left: 0;
      right: 0;
      bottom: 0;
      height: 0.25rem;
      background: rgba(255, 255, 255, 0.3);
    }
    .progress > span {
      display: block;
      height: 100%;
      background: var(--wa-color-brand-fill, #2563eb);
    }
    .title {
      font-weight: 600;
      font-size: 0.95rem;
      line-height: 1.3;
      overflow: hidden;
      display: -webkit-box;
      -webkit-line-clamp: 2;
      -webkit-box-orient: vertical;
    }
    .channel {
      font-size: 0.85rem;
      color: var(--wa-color-text-quiet);
    }
    .kebab {
      position: absolute;
      top: 0.5rem;
      right: 0.5rem;
      z-index: 2;
      width: 2rem;
      height: 2rem;
      border: 0;
      border-radius: 999px;
      background: rgba(0, 0, 0, 0.55);
      color: white;
      font-size: 1.1rem;
      line-height: 1;
      cursor: pointer;
      display: inline-flex;
      align-items: center;
      justify-content: center;
    }
    .kebab:hover,
    .kebab:focus-visible {
      background: rgba(0, 0, 0, 0.75);
      outline: none;
    }
    .menu {
      position: absolute;
      top: 2.75rem;
      right: 0.5rem;
      z-index: 3;
      background: var(--wa-color-surface-raised, white);
      border: 1px solid var(--wa-color-surface-border, #e5e7eb);
      border-radius: 0.375rem;
      box-shadow: 0 4px 12px rgba(0, 0, 0, 0.15);
      min-width: 10rem;
      padding: 0.25rem 0;
    }
    .menu button {
      display: block;
      width: 100%;
      text-align: left;
      background: none;
      border: 0;
      padding: 0.5rem 0.75rem;
      font: inherit;
      color: inherit;
      cursor: pointer;
    }
    .menu button:hover,
    .menu button:focus-visible {
      background: var(--wa-color-surface-border, #f3f4f6);
      outline: none;
    }
    .unhide-btn {
      align-self: flex-start;
      padding: 0.375rem 0.75rem;
      border: 1px solid var(--wa-color-surface-border, #d1d5db);
      border-radius: 0.375rem;
      background: var(--wa-color-surface-raised, white);
      cursor: pointer;
      font: inherit;
    }
    .unhide-btn[disabled] {
      opacity: 0.5;
      cursor: not-allowed;
    }
    .action-error {
      color: var(--wa-color-danger-fill, #b91c1c);
      font-size: 0.8rem;
      padding: 0 0.25rem;
    }
  `;

  private formatDuration(seconds: number): string {
    const s = Math.max(0, Math.round(seconds));
    const h = Math.floor(s / 3600);
    const m = Math.floor((s % 3600) / 60);
    const sec = s % 60;
    if (h > 0) return `${h}:${String(m).padStart(2, "0")}:${String(sec).padStart(2, "0")}`;
    return `${m}:${String(sec).padStart(2, "0")}`;
  }

  private toggleMenu(e: Event) {
    e.preventDefault();
    e.stopPropagation();
    this.menuOpen = !this.menuOpen;
  }

  override disconnectedCallback(): void {
    document.removeEventListener("click", this.onDocClick, true);
    document.removeEventListener("keydown", this.onDocKey, true);
    super.disconnectedCallback();
  }

  override updated(changed: Map<string, unknown>): void {
    if (changed.has("menuOpen")) {
      if (this.menuOpen) {
        document.addEventListener("click", this.onDocClick, true);
        document.addEventListener("keydown", this.onDocKey, true);
      } else {
        document.removeEventListener("click", this.onDocClick, true);
        document.removeEventListener("keydown", this.onDocKey, true);
      }
    }
  }

  private onDocClick = (e: MouseEvent) => {
    if (!e.composedPath().includes(this)) {
      this.menuOpen = false;
    }
  };

  private onDocKey = (e: KeyboardEvent) => {
    if (e.key === "Escape") {
      this.menuOpen = false;
    }
  };

  private async onHideClick(e: Event) {
    e.preventDefault();
    e.stopPropagation();
    if (!this.videoId || this.busy) return;
    this.busy = true;
    this.menuOpen = false;
    this.actionError = "";
    try {
      await api.post("/api/hidden", {
        video_id: this.videoId,
        video_title: this.title || null,
        channel_id: this.channelId,
        channel_title: this.channelTitle,
        video_thumbnail_url: this.thumbnailUrl,
        duration_seconds: this.duration,
      });
      this.dispatchEvent(
        new CustomEvent("video-hidden", {
          detail: { videoId: this.videoId, title: this.title },
          bubbles: true,
          composed: true,
        }),
      );
    } catch (err) {
      if (err instanceof ApiError) {
        console.warn("Failed to hide video", err.status, err.body);
        this.actionError = `Couldn't hide (${err.status})`;
      } else {
        console.warn("Failed to hide video", err);
        this.actionError = "Couldn't hide this video";
      }
    } finally {
      this.busy = false;
    }
  }

  private async onUnhideClick(e: Event) {
    e.preventDefault();
    e.stopPropagation();
    if (!this.videoId || this.busy) return;
    this.busy = true;
    this.actionError = "";
    try {
      await api.delete(`/api/hidden/${encodeURIComponent(this.videoId)}`);
      this.dispatchEvent(
        new CustomEvent("video-unhidden", {
          detail: { videoId: this.videoId },
          bubbles: true,
          composed: true,
        }),
      );
    } catch (err) {
      if (err instanceof ApiError) {
        console.warn("Failed to unhide video", err.status, err.body);
        this.actionError = `Couldn't unhide (${err.status})`;
      } else {
        console.warn("Failed to unhide video", err);
        this.actionError = "Couldn't unhide this video";
      }
    } finally {
      this.busy = false;
    }
  }

  private renderBody() {
    const pct = Math.max(0, Math.min(100, this.progress * 100));
    const thumbSrc = normalizeThumbnailUrl(this.thumbnailUrl);
    return html`
      <div class="thumb">
        ${thumbSrc ? html`<img src=${thumbSrc} alt="" loading="lazy" />` : null}
        ${this.duration != null
          ? html`<span class="duration">${this.formatDuration(this.duration)}</span>`
          : null}
        ${pct > 0
          ? html`<div class="progress">
              <span style="width: ${pct}%"></span>
            </div>`
          : null}
      </div>
      <div class="title">${this.title}</div>
      ${this.channelTitle ? html`<div class="channel">${this.channelTitle}</div>` : null}
    `;
  }

  override render() {
    if (this.mode === "hidden") {
      return html`
        <div class="card">
          <div class="card-body">${this.renderBody()}</div>
          <button
            class="unhide-btn"
            type="button"
            ?disabled=${this.busy}
            @click=${this.onUnhideClick}
          >
            Unhide
          </button>
          ${this.actionError
            ? html`<div class="action-error" role="alert">${this.actionError}</div>`
            : null}
        </div>
      `;
    }

    const href = this.videoId ? `/child/video/${this.videoId}` : "#";
    return html`
      <div class="card">
        <a class="card-link" href=${href} aria-label=${this.title}>${this.renderBody()}</a>
        <button
          class="kebab"
          type="button"
          aria-label="More actions"
          aria-haspopup="menu"
          aria-expanded=${this.menuOpen ? "true" : "false"}
          @click=${this.toggleMenu}
        >
          ⋮
        </button>
        ${this.menuOpen
          ? html`<div class="menu" role="menu">
              <button type="button" role="menuitem" @click=${this.onHideClick}>
                Hide this video
              </button>
            </div>`
          : null}
        ${this.actionError
          ? html`<div class="action-error" role="alert">${this.actionError}</div>`
          : null}
      </div>
    `;
  }
}

declare global {
  interface HTMLElementTagNameMap {
    "hometube-video-card": VideoCard;
  }
}
