/**
 * <hometube-preview-video video-id="...">
 *
 * Parent-side video preview. Mirrors the role
 * `<hometube-preview-channel>`
 * play but for a single video — fetches metadata via
 * `/api/preview/video/:id` (parent-only, allowlist-bypassed) and
 * renders the title, channel, and a thumbnail. Used inside the
 * allowlist-manager preview dialog so a parent can review a video
 * before adding it to a child's allowlist.
 *
 * The plan also envisaged embedding the actual `<hometube-video-player
 * preview>` here. For the allowlist flow the metadata-only render is
 * already enough to make a decision; embedding the full player is
 * possible but would require seeding `video_metadata_cache` on
 * preview, which we deliberately don't do (preview is supposed to
 * leave no trace).
 */

import { LitElement, html, css, nothing } from "lit";
import { customElement, property, state } from "lit/decorators.js";

import { ApiError, api } from "../services/api.js";
import { normalizeThumbnailUrl, type VideoMetadata } from "../types/index.js";

import "./loading-spinner.js";
import "./error-banner.js";

@customElement("hometube-preview-video")
export class PreviewVideo extends LitElement {
  @property({ type: String, attribute: "video-id" })
  videoId = "";

  @state() private data: VideoMetadata | null = null;
  @state() private loading = false;
  @state() private error = "";

  static styles = css`
    :host {
      display: block;
    }
    article {
      display: grid;
      gap: 0.75rem;
    }
    .thumb-wrap {
      position: relative;
      aspect-ratio: 16 / 9;
      background: var(--wa-color-surface-border);
      border-radius: 0.5rem;
      overflow: hidden;
      cursor: pointer;
    }
    .thumb-wrap:hover {
      opacity: 0.9;
    }
    .thumb-wrap img {
      width: 100%;
      height: 100%;
      object-fit: cover;
    }
    .thumb-wrap .play-icon {
      position: absolute;
      inset: 0;
      display: flex;
      align-items: center;
      justify-content: center;
      background: rgba(0, 0, 0, 0.3);
      opacity: 0;
      transition: opacity 0.15s;
      font-size: 2.5rem;
      color: white;
    }
    .thumb-wrap:hover .play-icon {
      opacity: 1;
    }
    h2 {
      margin: 0;
      font-size: 1.1rem;
    }
    .channel {
      color: var(--wa-color-text-quiet);
      font-size: 0.9rem;
    }
    .meta-row {
      display: flex;
      gap: 0.75rem;
      font-size: 0.85rem;
      color: var(--wa-color-text-quiet);
    }
  `;

  override connectedCallback(): void {
    super.connectedCallback();
    if (this.videoId) void this.load();
  }

  override updated(changed: Map<string, unknown>): void {
    if (changed.has("videoId") && this.videoId) {
      void this.load();
    }
  }

  private async load(): Promise<void> {
    this.loading = true;
    this.error = "";
    this.data = null;
    try {
      this.data = await api.get<VideoMetadata>(
        `/api/preview/video/${encodeURIComponent(this.videoId)}`,
      );
    } catch (err) {
      this.error = err instanceof ApiError ? String(err.body) : (err as Error).message;
    } finally {
      this.loading = false;
    }
  }

  override render() {
    if (this.loading) {
      return html`<hometube-loading-spinner
        label="Loading video preview…"
      ></hometube-loading-spinner>`;
    }
    if (this.error) {
      return html`<hometube-error-banner .message=${this.error}></hometube-error-banner>`;
    }
    if (!this.data) return nothing;

    const meta = this.data;
    const minutes = meta.duration_seconds !== null ? Math.floor(meta.duration_seconds / 60) : null;
    const seconds = meta.duration_seconds !== null ? meta.duration_seconds % 60 : null;
    const duration =
      minutes !== null && seconds !== null
        ? `${minutes}:${String(seconds).padStart(2, "0")}`
        : null;

    const previewHref = `/parent/preview/video/${encodeURIComponent(this.videoId)}`;
    return html`
      <article aria-label=${`Preview of ${meta.title ?? meta.id}`}>
        <a href=${previewHref} class="thumb-wrap" title="Watch full preview">
          ${normalizeThumbnailUrl(meta.thumbnail_url)
            ? html`<img src=${normalizeThumbnailUrl(meta.thumbnail_url)!} alt="" loading="lazy" />`
            : nothing}
          <div class="play-icon" aria-hidden="true">▶</div>
        </a>
        <div>
          <h2>${meta.title ?? meta.id}</h2>
          ${meta.channel_title ? html`<div class="channel">${meta.channel_title}</div>` : nothing}
          ${duration
            ? html`<div class="meta-row">
                <span>${duration}</span>
              </div>`
            : nothing}
        </div>
      </article>
    `;
  }
}

declare global {
  interface HTMLElementTagNameMap {
    "hometube-preview-video": PreviewVideo;
  }
}
