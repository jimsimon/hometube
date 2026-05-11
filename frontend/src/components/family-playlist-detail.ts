/**
 * <hometube-family-playlist-detail playlist-id="...">
 *
 * Parent-side editor for a single family playlist:
 *   - title / description / member-list edit (via the shared form)
 *   - add a video by ID (paste a YouTube URL or video id)
 *   - reorder videos via HTML5 drag-and-drop
 *   - remove a video
 */

import { LitElement, html, css, nothing } from "lit";
import { customElement, property, query, state } from "lit/decorators.js";

import { ApiError, api } from "../services/api.js";
import type { FamilyPlaylistDetail } from "../types/index.js";

import "./family-playlist-form.js";
import "./loading-spinner.js";
import "./error-banner.js";
import type { FamilyPlaylistForm } from "./family-playlist-form.js";

/** Extract a YouTube video id from a URL or pass-through the input. */
function extractVideoId(input: string): string {
  const trimmed = input.trim();
  if (!trimmed) return "";
  // youtu.be/<id> or watch?v=<id>
  try {
    const url = new URL(trimmed);
    if (url.hostname.endsWith("youtu.be")) {
      return url.pathname.replace(/^\//, "").split("/")[0] ?? "";
    }
    const v = url.searchParams.get("v");
    if (v) return v;
  } catch {
    // not a URL; fall through.
  }
  return trimmed;
}

@customElement("hometube-family-playlist-detail")
export class FamilyPlaylistDetailEl extends LitElement {
  @property({ type: Number, attribute: "playlist-id" })
  playlistId = 0;

  @state() private detail: FamilyPlaylistDetail | null = null;
  @state() private loading = false;
  @state() private error = "";
  @state() private addInput = "";
  @state() private adding = false;
  @state() private dragIndex: number | null = null;

  @query("hometube-family-playlist-form")
  private form!: FamilyPlaylistForm;

  static styles = css`
    :host {
      display: block;
    }
    header.detail-header {
      display: flex;
      gap: 1rem;
      align-items: flex-start;
      padding: 1rem;
      border: 1px solid var(--wa-color-surface-border);
      border-radius: 0.5rem;
      background: var(--wa-color-surface-default);
    }
    .meta {
      flex: 1;
    }
    .meta h1 {
      margin: 0 0 0.25rem;
    }
    .meta .description {
      color: var(--wa-color-text-quiet);
      white-space: pre-wrap;
    }
    .stats {
      color: var(--wa-color-text-quiet);
      font-size: 0.85rem;
    }
    .add-row {
      display: flex;
      gap: 0.5rem;
      margin-block: 1rem;
    }
    .add-row input {
      flex: 1;
      padding: 0.5rem;
      border: 1px solid var(--wa-color-surface-border, #ccc);
      border-radius: 0.375rem;
      background: var(--wa-color-surface-default);
      color: var(--wa-color-text-normal);
      font: inherit;
    }
    button.primary {
      padding: 0.5rem 1rem;
      border-radius: 0.375rem;
      border: 1px solid transparent;
      background: var(--wa-color-brand-fill, #2563eb);
      color: white;
      font: inherit;
      cursor: pointer;
    }
    button.secondary {
      padding: 0.4rem 0.75rem;
      border-radius: 0.375rem;
      border: 1px solid var(--wa-color-surface-border, #ccc);
      background: transparent;
      color: inherit;
      font: inherit;
      cursor: pointer;
    }
    ol {
      list-style: none;
      padding: 0;
      margin: 0;
      display: grid;
      gap: 0.5rem;
    }
    li {
      display: grid;
      grid-template-columns: auto auto 1fr auto;
      gap: 0.75rem;
      align-items: center;
      padding: 0.5rem;
      border: 1px solid var(--wa-color-surface-border);
      border-radius: 0.375rem;
      background: var(--wa-color-surface-default);
    }
    li.dragging {
      opacity: 0.55;
    }
    .grip {
      cursor: grab;
      padding: 0.25rem 0.5rem;
      color: var(--wa-color-text-quiet);
      user-select: none;
    }
    img {
      width: 8rem;
      height: 4.5rem;
      object-fit: cover;
      border-radius: 0.25rem;
      background: var(--wa-color-surface-border);
    }
    .row-meta {
      min-width: 0;
    }
    .row-title {
      font-weight: 600;
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
      padding: 1rem;
    }
  `;

  override connectedCallback(): void {
    super.connectedCallback();
    void this.load();
    this.addEventListener("hometube:family-playlist-saved", this.onSaved as EventListener);
  }

  override disconnectedCallback(): void {
    super.disconnectedCallback();
    this.removeEventListener("hometube:family-playlist-saved", this.onSaved as EventListener);
  }

  private onSaved = (): void => {
    void this.load();
  };

  private async load(): Promise<void> {
    this.loading = true;
    this.error = "";
    try {
      this.detail = await api.get<FamilyPlaylistDetail>(`/api/family-playlists/${this.playlistId}`);
    } catch (err) {
      this.error = err instanceof ApiError ? String(err.body) : (err as Error).message;
    } finally {
      this.loading = false;
    }
  }

  private openEdit = (): void => {
    if (this.detail) this.form?.open(this.detail);
  };

  private async addVideo(): Promise<void> {
    const videoId = extractVideoId(this.addInput);
    if (!videoId) return;
    this.adding = true;
    this.error = "";
    try {
      await api.post(`/api/family-playlists/${this.playlistId}/videos`, {
        video_id: videoId,
      });
      this.addInput = "";
      await this.load();
    } catch (err) {
      this.error = err instanceof ApiError ? String(err.body) : (err as Error).message;
    } finally {
      this.adding = false;
    }
  }

  private async removeVideo(videoId: string): Promise<void> {
    try {
      await api.delete(
        `/api/family-playlists/${this.playlistId}/videos/${encodeURIComponent(videoId)}`,
      );
      await this.load();
    } catch (err) {
      this.error = (err as Error).message;
    }
  }

  private onDragStart = (index: number) => (e: DragEvent) => {
    this.dragIndex = index;
    if (e.dataTransfer) {
      e.dataTransfer.effectAllowed = "move";
      e.dataTransfer.setData("text/plain", String(index));
    }
  };

  private onDragOver = (e: DragEvent): void => {
    e.preventDefault();
    if (e.dataTransfer) e.dataTransfer.dropEffect = "move";
  };

  private onDrop = (targetIndex: number) => (e: DragEvent) => {
    e.preventDefault();
    if (this.dragIndex == null || this.dragIndex === targetIndex || !this.detail) {
      this.dragIndex = null;
      return;
    }
    const videos = [...this.detail.videos];
    const [moved] = videos.splice(this.dragIndex, 1);
    if (moved) videos.splice(targetIndex, 0, moved);
    this.detail = { ...this.detail, videos };
    this.dragIndex = null;
    void this.pushReorder(videos.map((v) => v.video_id));
  };

  private async pushReorder(videoIds: string[]): Promise<void> {
    try {
      await api.put(`/api/family-playlists/${this.playlistId}/videos/reorder`, {
        video_ids: videoIds,
      });
    } catch (err) {
      this.error = (err as Error).message;
      void this.load();
    }
  }

  override render() {
    if (this.loading && this.detail == null) {
      return html`<hometube-loading-spinner label="Loading playlist…"></hometube-loading-spinner>`;
    }
    if (!this.detail) {
      return html`<hometube-error-banner
        message=${this.error || "Playlist not found."}
      ></hometube-error-banner>`;
    }

    return html`
      ${this.error
        ? html`<hometube-error-banner
            message=${this.error}
            dismissible
            @hometube:error-dismiss=${() => (this.error = "")}
          ></hometube-error-banner>`
        : nothing}
      <header class="detail-header">
        <div class="meta">
          <h1>${this.detail.title}</h1>
          ${this.detail.description
            ? html`<p class="description">${this.detail.description}</p>`
            : nothing}
          <p class="stats">
            ${this.detail.videos.length} videos · shared with ${this.detail.child_ids.length}
            child${this.detail.child_ids.length === 1 ? "" : "ren"}
          </p>
        </div>
        <button class="secondary" type="button" @click=${this.openEdit}>Edit</button>
      </header>

      <div class="add-row">
        <label class="sr-only" for="add-video-input">Video URL or ID</label>
        <input
          id="add-video-input"
          type="text"
          placeholder="Paste a YouTube URL or video ID"
          .value=${this.addInput}
          @input=${(e: Event) => (this.addInput = (e.target as HTMLInputElement).value)}
          @keydown=${(e: KeyboardEvent) => {
            if (e.key === "Enter") void this.addVideo();
          }}
        />
        <button
          class="primary"
          type="button"
          @click=${() => void this.addVideo()}
          ?disabled=${this.adding}
        >
          ${this.adding ? "Adding…" : "Add video"}
        </button>
      </div>

      ${this.detail.videos.length === 0
        ? html`<p class="empty">No videos in this playlist yet.</p>`
        : html`
            <ol aria-label="Playlist videos">
              ${this.detail.videos.map(
                (v, i) => html`
                  <li
                    draggable="true"
                    class=${this.dragIndex === i ? "dragging" : ""}
                    @dragstart=${this.onDragStart(i)}
                    @dragover=${this.onDragOver}
                    @drop=${this.onDrop(i)}
                  >
                    <span class="grip" aria-hidden="true">⋮⋮</span>
                    ${v.video_thumbnail_url
                      ? html`<img src=${v.video_thumbnail_url} alt="" />`
                      : html`<div></div>`}
                    <div class="row-meta">
                      <div class="row-title">${v.video_title}</div>
                      ${v.channel_title
                        ? html`<div class="row-channel">${v.channel_title}</div>`
                        : nothing}
                    </div>
                    <button
                      class="secondary"
                      type="button"
                      aria-label=${`Remove ${v.video_title}`}
                      @click=${() => void this.removeVideo(v.video_id)}
                    >
                      Remove
                    </button>
                  </li>
                `,
              )}
            </ol>
          `}

      <hometube-family-playlist-form></hometube-family-playlist-form>
    `;
  }
}

declare global {
  interface HTMLElementTagNameMap {
    "hometube-family-playlist-detail": FamilyPlaylistDetailEl;
  }
}
