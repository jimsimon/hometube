/**
 * <hometube-allowlist-manager child-id="...">
 *
 * Three-tab UI (channels / playlists / videos) for managing what a
 * single child can see. Each tab combines:
 *   - a YouTube search box (parent-side: /api/parent/search)
 *   - a list of allowlisted items with remove buttons
 *
 * `child-id` is set externally by the parent home page when the child
 * dropdown changes.
 */

import { LitElement, html, css, nothing } from "lit";
import { customElement, property, query, state } from "lit/decorators.js";

import { ApiError, api } from "../services/api.js";
import {
  pickThumbnail,
  type AllowlistedChannel,
  type AllowlistedPlaylist,
  type AllowlistedVideo,
  type SearchItem,
  type SearchResponse,
} from "../types/index.js";

import "./preview-channel.js";
import "./preview-playlist.js";
import "./preview-video.js";
import "./loading-spinner.js";
import "./error-banner.js";

type Kind = "channel" | "playlist" | "video";

@customElement("hometube-allowlist-manager")
export class AllowlistManager extends LitElement {
  @property({ type: Number, attribute: "child-id" })
  childId: number | null = null;

  @state() private activeTab: Kind = "channel";
  @state() private channels: AllowlistedChannel[] = [];
  @state() private playlists: AllowlistedPlaylist[] = [];
  @state() private videos: AllowlistedVideo[] = [];
  @state() private searchQ = "";
  @state() private searchResults: SearchItem[] = [];
  @state() private searching = false;
  @state() private error = "";
  /**
   * Currently-previewed search result. When non-null the preview
   * `<wa-dialog>` is open and showing the corresponding component
   * (channel / playlist / video). The dialog closes when this goes
   * back to `null` — either via the explicit close button, the
   * `wa-after-hide` event, or after a successful "Add to allowlist".
   */
  @state() private previewItem: SearchItem | null = null;
  @state() private previewKind: Kind = "channel";
  @state() private addingFromPreview = false;

  @query("wa-dialog.preview-dialog") private previewDialog!: HTMLElement & {
    open?: boolean;
    show?: () => void;
    hide?: () => void;
  };

  static styles = css`
    :host {
      display: block;
    }
    .row {
      display: flex;
      gap: 0.5rem;
      flex-wrap: wrap;
      align-items: center;
      margin-block: 1rem;
    }
    input[type="search"],
    input[type="text"] {
      flex: 1 1 16rem;
      padding: 0.5rem;
      border: 1px solid var(--wa-color-surface-border, #ccc);
      border-radius: 0.375rem;
      background: var(--wa-color-surface-default);
      color: var(--wa-color-text-normal);
      font: inherit;
    }
    button {
      padding: 0.5rem 0.75rem;
      border-radius: 0.375rem;
      border: 1px solid var(--wa-color-surface-border, #ccc);
      background: var(--wa-color-brand-fill, #2563eb);
      color: white;
      font: inherit;
      cursor: pointer;
    }
    button.secondary {
      background: transparent;
      color: var(--wa-color-text-normal);
    }
    .grid {
      display: grid;
      gap: 0.75rem;
      grid-template-columns: repeat(auto-fill, minmax(min(14rem, 100%), 1fr));
    }
    .card {
      display: flex;
      gap: 0.75rem;
      padding: 0.75rem;
      border: 1px solid var(--wa-color-surface-border);
      border-radius: 0.5rem;
      background: var(--wa-color-surface-raised, transparent);
    }
    .card img {
      width: 6rem;
      height: 4rem;
      object-fit: cover;
      border-radius: 0.25rem;
      flex-shrink: 0;
      background: var(--wa-color-surface-border);
    }
    .card .meta {
      display: flex;
      flex-direction: column;
      gap: 0.25rem;
      min-width: 0;
    }
    .card .meta strong {
      font-size: 0.95rem;
      overflow: hidden;
      text-overflow: ellipsis;
      display: -webkit-box;
      -webkit-line-clamp: 2;
      -webkit-box-orient: vertical;
    }
    .empty {
      color: var(--wa-color-text-quiet);
      font-style: italic;
    }
    .error {
      color: var(--wa-color-danger-fill, #b91c1c);
    }
  `;

  override updated(changed: Map<string, unknown>): void {
    if (changed.has("childId") && this.childId != null) {
      void this.refreshAll();
    }
  }

  private async refreshAll(): Promise<void> {
    if (this.childId == null) return;
    try {
      const [c, p, v] = await Promise.all([
        api.get<AllowlistedChannel[]>(`/api/children/${this.childId}/allowlist/channels`),
        api.get<AllowlistedPlaylist[]>(`/api/children/${this.childId}/allowlist/playlists`),
        api.get<AllowlistedVideo[]>(`/api/children/${this.childId}/allowlist/videos`),
      ]);
      this.channels = c;
      this.playlists = p;
      this.videos = v;
    } catch (err) {
      this.error = (err as Error).message;
    }
  }

  private setTab(kind: Kind): void {
    this.activeTab = kind;
    this.searchResults = [];
    this.searchQ = "";
  }

  private async runSearch(): Promise<void> {
    if (!this.searchQ.trim()) return;
    this.searching = true;
    this.error = "";
    try {
      const res = await api.get<SearchResponse>(
        `/api/parent/search?q=${encodeURIComponent(this.searchQ)}&type=${this.activeTab}`,
      );
      this.searchResults = res.items;
    } catch (err) {
      this.error = err instanceof ApiError ? String(err.body) : (err as Error).message;
    } finally {
      this.searching = false;
    }
  }

  private async addItem(item: SearchItem): Promise<void> {
    await this.addItemForKind(item, this.activeTab);
  }

  private async addItemForKind(item: SearchItem, kind: Kind): Promise<void> {
    if (this.childId == null) return;
    const base = `/api/children/${this.childId}/allowlist/${kind}s`;
    const payload =
      kind === "channel"
        ? { channel_id: item.id }
        : kind === "playlist"
          ? { playlist_id: item.id }
          : { video_id: item.id };
    try {
      await api.post(base, payload);
      await this.refreshAll();
    } catch (err) {
      this.error = (err as Error).message;
    }
  }

  private openPreview(item: SearchItem): void {
    this.previewItem = item;
    this.previewKind = this.activeTab;
    queueMicrotask(() => this.previewDialog?.show?.());
  }

  private closePreview = (): void => {
    this.previewDialog?.hide?.();
    this.previewItem = null;
    this.addingFromPreview = false;
  };

  private async addFromPreview(): Promise<void> {
    if (!this.previewItem) return;
    this.addingFromPreview = true;
    try {
      await this.addItemForKind(this.previewItem, this.previewKind);
      this.closePreview();
    } finally {
      this.addingFromPreview = false;
    }
  }

  private async removeItem(id: string): Promise<void> {
    if (this.childId == null) return;
    const base = `/api/children/${this.childId}/allowlist/${this.activeTab}s/${encodeURIComponent(id)}`;
    try {
      await api.delete(base);
      await this.refreshAll();
    } catch (err) {
      this.error = (err as Error).message;
    }
  }

  override render() {
    if (this.childId == null) {
      return html`<p class="empty">Pick a child to manage their allowlist.</p>`;
    }
    return html`
      <div role="tablist" class="row">
        ${(["channel", "playlist", "video"] as Kind[]).map(
          (k) => html`
            <button
              role="tab"
              class=${this.activeTab === k ? "" : "secondary"}
              aria-selected=${this.activeTab === k ? "true" : "false"}
              @click=${() => this.setTab(k)}
            >
              ${k.charAt(0).toUpperCase() + k.slice(1)}s
            </button>
          `,
        )}
      </div>

      <div class="row">
        <label for="allowlist-search" class="sr-only">Search YouTube</label>
        <input
          id="allowlist-search"
          type="search"
          placeholder=${`Search ${this.activeTab}s on YouTube`}
          .value=${this.searchQ}
          @input=${(e: Event) => (this.searchQ = (e.target as HTMLInputElement).value)}
          @keydown=${(e: KeyboardEvent) => {
            if (e.key === "Enter") void this.runSearch();
          }}
        />
        <button @click=${() => void this.runSearch()} ?disabled=${this.searching}>
          ${this.searching ? "Searching…" : "Search"}
        </button>
      </div>

      ${this.error
        ? html`<hometube-error-banner .message=${this.error}></hometube-error-banner>`
        : nothing}
      ${this.searchResults.length > 0
        ? html`
            <h3>Add a result</h3>
            <div class="grid">
              ${this.searchResults.map(
                (item) => html`
                  <div class="card">
                    <img src=${pickThumbnail(item.thumbnails) ?? ""} alt="" loading="lazy" />
                    <div class="meta">
                      <strong>${item.title}</strong>
                      <span class="empty">${item.channel_title ?? ""}</span>
                      <div style="display: flex; gap: 0.25rem; flex-wrap: wrap;">
                        <button
                          type="button"
                          class="secondary"
                          aria-label=${`Preview ${item.title}`}
                          @click=${() => this.openPreview(item)}
                        >
                          Preview
                        </button>
                        <button
                          type="button"
                          class="secondary"
                          @click=${() => void this.addItem(item)}
                        >
                          Add to allowlist
                        </button>
                      </div>
                    </div>
                  </div>
                `,
              )}
            </div>
          `
        : nothing}

      <h3>Allowlisted ${this.activeTab}s</h3>
      ${this.renderCurrent()} ${this.renderPreviewDialog()}
    `;
  }

  private renderPreviewDialog() {
    const item = this.previewItem;
    const dialogLabel = item ? `Preview ${this.previewKind}: ${item.title}` : "Preview";
    return html`
      <wa-dialog
        class="preview-dialog"
        label=${dialogLabel}
        aria-label=${dialogLabel}
        @wa-after-hide=${this.closePreview}
      >
        ${item
          ? this.previewKind === "channel"
            ? html`<hometube-preview-channel channel-id=${item.id}></hometube-preview-channel>`
            : this.previewKind === "playlist"
              ? html`<hometube-preview-playlist playlist-id=${item.id}></hometube-preview-playlist>`
              : html`<hometube-preview-video video-id=${item.id}></hometube-preview-video>`
          : nothing}
        <div
          style="display: flex; gap: 0.5rem; justify-content: flex-end; margin-top: 1rem;"
          slot="footer"
        >
          <button
            type="button"
            class="secondary"
            @click=${this.closePreview}
            ?disabled=${this.addingFromPreview}
          >
            Close
          </button>
          <button
            type="button"
            @click=${() => void this.addFromPreview()}
            ?disabled=${this.addingFromPreview}
          >
            ${this.addingFromPreview ? "Adding…" : "Add to allowlist"}
          </button>
        </div>
      </wa-dialog>
    `;
  }

  private renderCurrent() {
    if (this.activeTab === "channel") {
      if (this.channels.length === 0) return html`<p class="empty">No channels yet.</p>`;
      return html`
        <div class="grid">
          ${this.channels.map(
            (c) => html`
              <div class="card">
                <img src=${c.channel_thumbnail_url ?? ""} alt="" loading="lazy" />
                <div class="meta">
                  <strong>${c.channel_title}</strong>
                  <button
                    type="button"
                    class="secondary"
                    @click=${() => void this.removeItem(c.channel_id)}
                  >
                    Remove
                  </button>
                </div>
              </div>
            `,
          )}
        </div>
      `;
    }
    if (this.activeTab === "playlist") {
      if (this.playlists.length === 0) return html`<p class="empty">No playlists yet.</p>`;
      return html`
        <div class="grid">
          ${this.playlists.map(
            (p) => html`
              <div class="card">
                <img src=${p.playlist_thumbnail_url ?? ""} alt="" loading="lazy" />
                <div class="meta">
                  <strong>${p.playlist_title}</strong>
                  <button
                    type="button"
                    class="secondary"
                    @click=${() => void this.removeItem(p.playlist_id)}
                  >
                    Remove
                  </button>
                </div>
              </div>
            `,
          )}
        </div>
      `;
    }
    if (this.videos.length === 0) return html`<p class="empty">No videos yet.</p>`;
    return html`
      <div class="grid">
        ${this.videos.map(
          (v) => html`
            <div class="card">
              <img src=${v.video_thumbnail_url ?? ""} alt="" loading="lazy" />
              <div class="meta">
                <strong>${v.video_title}</strong>
                <span class="empty">${v.channel_title ?? ""}</span>
                <button
                  type="button"
                  class="secondary"
                  @click=${() => void this.removeItem(v.video_id)}
                >
                  Remove
                </button>
              </div>
            </div>
          `,
        )}
      </div>
    `;
  }
}

declare global {
  interface HTMLElementTagNameMap {
    "hometube-allowlist-manager": AllowlistManager;
  }
}
