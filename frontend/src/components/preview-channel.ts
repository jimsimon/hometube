/**
 * <hometube-preview-channel channel-id="..." [child-id="..."]>
 *
 * Parent-side channel page rendered on /parent/preview/channel/:id and
 * inside the allowlist manager's preview dialog. Calls
 * /api/preview/channel/:id (allowlist bypassed) and shows a thumbnail
 * grid of recent uploads. Each thumbnail links to the per-video watch
 * page so the parent can check content before allowing it.
 *
 * When `child-id` is set the grid becomes an allowlist picker: every
 * upload gets an "Add" button plus a checkbox, and a toolbar adds all
 * selected videos in one go. This is how a parent allows a handful of
 * videos from a channel *without* allowlisting the whole channel. Videos
 * already on that child's allowlist render as "Added". Each successful
 * batch dispatches a bubbling, composed `allowlist-videos-added` event
 * (`detail: { videoIds }`) so the hosting allowlist manager can refresh.
 */

import { LitElement, html, css, nothing } from "lit";
import { customElement, property, state } from "lit/decorators.js";

import { ApiError, api } from "../services/api.js";
import {
  pickThumbnail,
  type AllowlistedVideo,
  type ChannelPreview,
  type ChannelVideoItem,
} from "../types/index.js";

import "./loading-spinner.js";
import "./error-banner.js";

@customElement("hometube-preview-channel")
export class PreviewChannel extends LitElement {
  @property({ type: String, attribute: "channel-id" })
  channelId = "";

  /**
   * Child whose allowlist the per-video "Add" controls target. When
   * unset the component is read-only (plain preview grid).
   */
  @property({ type: Number, attribute: "child-id" })
  childId: number | null = null;

  @state() private data: ChannelPreview | null = null;
  @state() private loading = false;
  @state() private error = "";
  /** Video IDs already on the child's allowlist (only when `childId` is set). */
  @state() private allowlisted: Set<string> = new Set();
  /** Checked-but-not-yet-added video IDs. */
  @state() private selected: Set<string> = new Set();
  /** Video IDs with an add request in flight. */
  @state() private adding: Set<string> = new Set();
  @state() private addError = "";

  static styles = css`
    :host {
      display: block;
    }
    header {
      display: flex;
      gap: 1rem;
      align-items: center;
      padding: 1rem;
      border: 1px solid var(--wa-color-surface-border);
      border-radius: 0.5rem;
      background: var(--wa-color-surface-default);
    }
    header img {
      width: 4rem;
      height: 4rem;
      border-radius: 50%;
      object-fit: cover;
    }
    h1 {
      margin: 0 0 0.25rem;
    }
    .description {
      color: var(--wa-color-text-quiet);
      white-space: pre-wrap;
      font-size: 0.95rem;
    }
    .grid-heading {
      display: flex;
      flex-wrap: wrap;
      gap: 0.5rem 1rem;
      align-items: center;
      justify-content: space-between;
      margin: 1rem 0 0.5rem;
    }
    .grid-heading h2 {
      font-size: 1rem;
      margin: 0;
    }
    .toolbar {
      display: flex;
      flex-wrap: wrap;
      gap: 0.5rem;
      align-items: center;
    }
    .toolbar .hint {
      font-size: 0.85rem;
      color: var(--wa-color-text-quiet);
    }
    .grid {
      display: grid;
      gap: 0.75rem;
      grid-template-columns: repeat(auto-fill, minmax(min(15rem, 100%), 1fr));
    }
    .card {
      display: grid;
      gap: 0.4rem;
      padding: 0.5rem;
      border: 1px solid var(--wa-color-surface-border);
      border-radius: 0.5rem;
      background: var(--wa-color-surface-default);
      color: inherit;
    }
    .card.selected {
      border-color: var(--wa-color-brand-fill, #2563eb);
      box-shadow: 0 0 0 1px var(--wa-color-brand-fill, #2563eb);
    }
    .card a {
      display: block;
      text-decoration: none;
      color: inherit;
    }
    .card img,
    .card .thumb-placeholder {
      width: 100%;
      aspect-ratio: 16 / 9;
      object-fit: cover;
      border-radius: 0.375rem;
      background: var(--wa-color-surface-border);
    }
    .card .title {
      font-weight: 600;
      font-size: 0.95rem;
      overflow: hidden;
      display: -webkit-box;
      -webkit-line-clamp: 2;
      -webkit-box-orient: vertical;
    }
    .card .actions {
      display: flex;
      align-items: center;
      justify-content: space-between;
      gap: 0.5rem;
    }
    .card label {
      display: inline-flex;
      align-items: center;
      gap: 0.35rem;
      font-size: 0.85rem;
      cursor: pointer;
    }
    .card input[type="checkbox"] {
      width: 1.1rem;
      height: 1.1rem;
      accent-color: var(--wa-color-brand-fill, #2563eb);
    }
    .added {
      font-size: 0.85rem;
      color: var(--wa-color-success-on-quiet, var(--wa-color-text-quiet));
      font-weight: 600;
    }
    .empty {
      color: var(--wa-color-text-quiet);
      font-style: italic;
      padding: 1rem 0;
    }
  `;

  override connectedCallback(): void {
    super.connectedCallback();
    if (this.channelId) void this.load();
  }

  override willUpdate(changed: Map<string, unknown>): void {
    // Reset picker state synchronously (before render) when the target
    // child changes, so no extra update cycle is scheduled.
    if (changed.has("childId")) {
      this.selected = new Set();
      if (this.childId == null) this.allowlisted = new Set();
    }
  }

  override updated(changed: Map<string, unknown>): void {
    if (changed.has("channelId") && this.channelId) {
      void this.load();
    }
    if (changed.has("childId") && this.childId != null) {
      void this.loadAllowlisted();
    }
  }

  private async load(): Promise<void> {
    this.loading = true;
    this.error = "";
    try {
      this.data = await api.get<ChannelPreview>(
        `/api/preview/channel/${encodeURIComponent(this.channelId)}`,
      );
    } catch (err) {
      this.error = err instanceof ApiError ? String(err.body) : (err as Error).message;
    } finally {
      this.loading = false;
    }
  }

  /** Learn which uploads are already allowlisted for `childId`. */
  private async loadAllowlisted(): Promise<void> {
    if (this.childId == null) return;
    try {
      const rows = await api.get<AllowlistedVideo[]>(
        `/api/children/${this.childId}/allowlist/videos`,
      );
      this.allowlisted = new Set(rows.map((r) => r.video_id));
    } catch {
      // Non-fatal: the picker still works, it just can't grey out
      // videos that are already allowed.
      this.allowlisted = new Set();
    }
  }

  private toggleSelected(videoId: string, checked: boolean): void {
    const next = new Set(this.selected);
    if (checked) next.add(videoId);
    else next.delete(videoId);
    this.selected = next;
  }

  /** Every upload that can still be added (not allowlisted, not in flight). */
  private addableVideos(): ChannelVideoItem[] {
    return (this.data?.videos ?? []).filter(
      (v) => !this.allowlisted.has(v.video_id) && !this.adding.has(v.video_id),
    );
  }

  private selectAll(): void {
    this.selected = new Set(this.addableVideos().map((v) => v.video_id));
  }

  private clearSelection(): void {
    this.selected = new Set();
  }

  private async addSelected(): Promise<void> {
    const ids = this.addableVideos()
      .map((v) => v.video_id)
      .filter((id) => this.selected.has(id));
    await this.addVideos(ids);
  }

  /**
   * POST each video to the child's allowlist, sequentially so the
   * backend's per-video sidecar lookup isn't hammered in parallel.
   * Successful ids move from `selected` to `allowlisted`; failures stay
   * selected and surface a single error line.
   */
  private async addVideos(videoIds: string[]): Promise<void> {
    if (this.childId == null || videoIds.length === 0) return;
    const byId = new Map((this.data?.videos ?? []).map((v) => [v.video_id, v]));
    const channelTitle = this.data?.title ?? null;
    this.addError = "";
    this.adding = new Set([...this.adding, ...videoIds]);

    const added: string[] = [];
    const failed: string[] = [];
    for (const id of videoIds) {
      const v = byId.get(id);
      if (!v) continue;
      try {
        // Forward the metadata we already have so the row is searchable
        // even if the sidecar can't resolve the video right now.
        await api.post(`/api/children/${this.childId}/allowlist/videos`, {
          video_id: v.video_id,
          title: v.title,
          channel_title: v.channel_title ?? channelTitle,
          thumbnail_url: pickThumbnail(v.thumbnails),
        });
        added.push(id);
      } catch {
        failed.push(id);
      }
    }

    const nextAdding = new Set(this.adding);
    for (const id of videoIds) nextAdding.delete(id);
    this.adding = nextAdding;
    if (added.length > 0) {
      this.allowlisted = new Set([...this.allowlisted, ...added]);
      const nextSelected = new Set(this.selected);
      for (const id of added) nextSelected.delete(id);
      this.selected = nextSelected;
      this.dispatchEvent(
        new CustomEvent("allowlist-videos-added", {
          bubbles: true,
          composed: true,
          detail: { childId: this.childId, videoIds: added },
        }),
      );
    }
    if (failed.length > 0) {
      this.addError =
        failed.length === 1
          ? `Couldn't add “${byId.get(failed[0])?.title ?? failed[0]}”. Try again.`
          : `Couldn't add ${failed.length} videos. Try again.`;
    }
  }

  override render() {
    if (this.loading && !this.data) {
      return html`<hometube-loading-spinner></hometube-loading-spinner>`;
    }
    if (this.error) {
      return html`<hometube-error-banner message=${this.error}></hometube-error-banner>`;
    }
    if (!this.data) return nothing;

    const thumb = pickThumbnail(this.data.thumbnails);
    const picker = this.childId != null;
    const videos = this.data.videos;
    const selectedCount = this.addableVideos().filter((v) => this.selected.has(v.video_id)).length;
    const busy = this.adding.size > 0;
    return html`
      <header>
        ${thumb ? html`<img src=${thumb} alt="" />` : nothing}
        <div>
          <h1>${this.data.title}</h1>
          ${this.data.description
            ? html`<p class="description">${this.data.description}</p>`
            : nothing}
        </div>
      </header>

      ${videos.length > 0
        ? html`
            <div class="grid-heading">
              <h2>Recent uploads (${videos.length})</h2>
              ${picker
                ? html`
                    <div class="toolbar" role="group" aria-label="Add selected videos">
                      <span class="hint">
                        ${selectedCount === 0
                          ? "Tick videos to allow just those, without the whole channel."
                          : `${selectedCount} selected`}
                      </span>
                      <wa-button
                        size="small"
                        variant="default"
                        ?disabled=${busy || this.addableVideos().length === 0}
                        @click=${() =>
                          selectedCount === this.addableVideos().length
                            ? this.clearSelection()
                            : this.selectAll()}
                      >
                        ${selectedCount > 0 && selectedCount === this.addableVideos().length
                          ? "Clear selection"
                          : "Select all"}
                      </wa-button>
                      <wa-button
                        size="small"
                        variant="brand"
                        class="add-selected"
                        ?disabled=${busy || selectedCount === 0}
                        @click=${() => void this.addSelected()}
                      >
                        ${busy
                          ? "Adding…"
                          : `Add ${selectedCount > 0 ? selectedCount : ""} selected to allowlist`}
                      </wa-button>
                    </div>
                  `
                : nothing}
            </div>
            ${this.addError
              ? html`<hometube-error-banner .message=${this.addError}></hometube-error-banner>`
              : nothing}
            <div class="grid">${videos.map((v) => this.renderCard(v, picker))}</div>
          `
        : html`<p class="empty">No videos found for this channel.</p>`}
    `;
  }

  private renderCard(v: ChannelVideoItem, picker: boolean) {
    const thumbUrl = pickThumbnail(v.thumbnails);
    const isAdded = this.allowlisted.has(v.video_id);
    const isAdding = this.adding.has(v.video_id);
    const isSelected = this.selected.has(v.video_id) && !isAdded;
    return html`
      <div class="card ${isSelected ? "selected" : ""}" data-video-id=${v.video_id}>
        <a href="/parent/preview/video/${encodeURIComponent(v.video_id)}" title="Watch">
          ${thumbUrl
            ? html`<img src=${thumbUrl} alt="" loading="lazy" />`
            : html`<div class="thumb-placeholder"></div>`}
          <div class="title">${v.title}</div>
        </a>
        ${picker
          ? html`
              <div class="actions">
                ${isAdded
                  ? html`<span class="added">✓ Added</span>`
                  : html`
                      <label>
                        <input
                          type="checkbox"
                          .checked=${isSelected}
                          ?disabled=${isAdding}
                          aria-label=${`Select ${v.title}`}
                          @change=${(e: Event) =>
                            this.toggleSelected(v.video_id, (e.target as HTMLInputElement).checked)}
                        />
                        Select
                      </label>
                      <wa-button
                        size="small"
                        variant="default"
                        class="add-one"
                        ?disabled=${isAdding}
                        aria-label=${`Add ${v.title} to allowlist`}
                        @click=${() => void this.addVideos([v.video_id])}
                      >
                        ${isAdding ? "Adding…" : "Add"}
                      </wa-button>
                    `}
              </div>
            `
          : nothing}
      </div>
    `;
  }
}

declare global {
  interface HTMLElementTagNameMap {
    "hometube-preview-channel": PreviewChannel;
  }
}
