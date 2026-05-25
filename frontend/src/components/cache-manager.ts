/**
 * <hometube-cache-manager>
 *
 * Parent-only segment-cache control panel. Shows:
 *   - total cache size + segment count + hit rate
 *   - max-size dropdown (preset list from the backend)
 *   - list of cached videos with per-video evict button
 *   - "Clear all" button (confirmation in <wa-dialog> with focus trap)
 *
 * All mutations call back into `/api/cache/*` and re-fetch the stats
 * on success. State updates announce themselves via an internal ARIA
 * live region.
 */

import { LitElement, html, css, nothing } from "lit";
import { customElement, state, query } from "lit/decorators.js";

import { api } from "../services/api.js";

interface CacheStats {
  total_bytes: number;
  segment_count: number;
  video_count: number;
  hit_count: number;
  miss_count: number;
  hit_rate: number;
  max_size_label: string;
  max_size_bytes: number;
  unlimited: boolean;
  top_videos: Array<{
    video_id: string;
    total_bytes: number;
    segment_count: number;
  }>;
  /** Parallel thumbnail cache populated by the proxy route on miss
   *  and by the channel-backfill tail-call. Lives on the same
   *  payload so the panel can render both caches in one round-trip. */
  thumbnail_cache: ThumbnailCacheSummary;
}

interface ThumbnailCacheSummary {
  entry_count: number;
  total_bytes: number;
  /** Configured LRU cap in bytes. `0` = unlimited (no eviction). */
  max_bytes: number;
}

interface CacheSettings {
  max_size: string;
  metadata_ttl_hours: number;
}

interface EvictionEntry {
  id: number;
  video_id: string;
  segment_count: number;
  bytes_freed: number;
  reason: string;
  evicted_at: number;
}

// Must stay in sync with `CACHE_SIZE_PRESETS` in
// `src/services/video_cache.rs` — the backend rejects unknown labels.
const PRESETS = ["10 GB", "25 GB", "50 GB", "100 GB", "250 GB", "500 GB", "Unlimited"];

const REASON_LABELS: Record<string, string> = {
  manual: "Manually cleared",
  clear_all: "Entire cache cleared",
  not_allowlisted: "Not on any allowlist",
  lru_size_limit: "Over cache size limit",
};

@customElement("hometube-cache-manager")
export class CacheManager extends LitElement {
  @state() private stats: CacheStats | null = null;
  @state() private settings: CacheSettings | null = null;
  @state() private evictions: EvictionEntry[] = [];
  @state() private busy = false;
  @state() private status = "";
  @state() private confirmOpen = false;
  @state() private videoIdInput = "";

  @query("wa-dialog") private dialog!: HTMLElement & {
    show?: () => void;
    hide?: () => void;
    open?: boolean;
  };
  @query("button.cancel-confirm") private cancelButton?: HTMLButtonElement;

  static styles = css`
    :host {
      display: block;
      margin-bottom: 1rem;
    }
    article {
      padding: 1rem;
      border: 1px solid var(--wa-color-surface-border);
      border-radius: 0.5rem;
      background: var(--wa-color-surface-default);
    }
    dl {
      display: grid;
      grid-template-columns: max-content 1fr;
      gap: 0.25rem 1rem;
      margin: 0.5rem 0;
    }
    dt {
      font-weight: 600;
    }
    .controls {
      display: flex;
      gap: 0.75rem;
      align-items: center;
      flex-wrap: wrap;
      margin: 1rem 0;
    }
    select,
    button {
      padding: 0.4rem 0.75rem;
      border-radius: 0.375rem;
      border: 1px solid var(--wa-color-surface-border, #ccc);
      background: var(--wa-color-surface-default);
      color: var(--wa-color-text-normal);
      font: inherit;
    }
    button {
      cursor: pointer;
    }
    button.danger {
      background: var(--wa-color-danger-fill, #b91c1c);
      color: white;
      border-color: transparent;
    }
    table {
      width: 100%;
      border-collapse: collapse;
      margin-top: 1rem;
    }
    th,
    td {
      text-align: left;
      padding: 0.5rem;
      border-bottom: 1px solid var(--wa-color-surface-border);
    }
    th {
      font-size: 0.85rem;
      color: var(--wa-color-text-quiet);
    }
    .progress {
      height: 0.5rem;
      background: var(--wa-color-surface-raised);
      border-radius: 999px;
      overflow: hidden;
      margin: 0.25rem 0;
    }
    .progress > span {
      display: block;
      height: 100%;
      background: var(--wa-color-brand-fill, #2563eb);
    }
    .live {
      position: absolute;
      width: 1px;
      height: 1px;
      overflow: hidden;
      clip: rect(0 0 0 0);
      white-space: nowrap;
    }
    .evictions-heading {
      margin: 1.5rem 0 0.25rem;
      font-size: 1rem;
    }
    .hint {
      display: block;
      font-weight: 400;
      font-size: 0.85rem;
      color: var(--wa-color-text-quiet);
      margin-top: 0.15rem;
    }
    .clear-by-id {
      margin: 1rem 0;
      padding: 0.75rem 1rem;
      border: 1px solid var(--wa-color-surface-border);
      border-radius: 0.375rem;
      background: var(--wa-color-surface-raised);
    }
    .clear-by-id label {
      display: block;
      font-weight: 600;
      margin-bottom: 0.5rem;
    }
    .clear-by-id-row {
      display: flex;
      gap: 0.5rem;
      align-items: center;
      flex-wrap: wrap;
    }
    .clear-by-id input {
      flex: 1 1 14rem;
      padding: 0.4rem 0.6rem;
      border-radius: 0.375rem;
      border: 1px solid var(--wa-color-surface-border, #ccc);
      background: var(--wa-color-surface-default);
      color: var(--wa-color-text-normal);
      font: inherit;
      font-family: ui-monospace, SFMono-Regular, Menlo, monospace;
    }
    .dialog-actions {
      display: flex;
      gap: 0.5rem;
      justify-content: flex-end;
      margin-top: 1rem;
    }
  `;

  override connectedCallback(): void {
    super.connectedCallback();
    void this.load();
  }

  private async load(): Promise<void> {
    try {
      const [stats, settings, evictions] = await Promise.all([
        api.get<CacheStats>("/api/cache/stats"),
        api.get<CacheSettings>("/api/cache/settings"),
        api.get<EvictionEntry[]>("/api/cache/evictions?limit=50"),
      ]);
      this.stats = stats;
      this.settings = settings;
      this.evictions = evictions;
    } catch (err) {
      this.status = `Failed to load cache: ${(err as Error).message}`;
    }
  }

  private fmtTimestamp(ts: number): string {
    // `evicted_at` is unix seconds.
    return new Date(ts * 1000).toLocaleString();
  }

  private fmtReason(reason: string): string {
    return REASON_LABELS[reason] ?? reason;
  }

  private fmtBytes(bytes: number): string {
    if (bytes === 0) return "0 B";
    const units = ["B", "KB", "MB", "GB", "TB"];
    const i = Math.min(units.length - 1, Math.floor(Math.log(bytes) / Math.log(1024)));
    return `${(bytes / Math.pow(1024, i)).toFixed(1)} ${units[i]}`;
  }

  private async onMaxSize(e: Event): Promise<void> {
    const max_size = (e.target as HTMLSelectElement).value;
    this.busy = true;
    try {
      this.settings = await api.put<CacheSettings>("/api/cache/settings", {
        max_size,
      });
      this.status = `Max size set to ${max_size}.`;
      await this.load();
    } catch (err) {
      this.status = `Failed: ${(err as Error).message}`;
    } finally {
      this.busy = false;
    }
  }

  private async onEvict(videoId: string): Promise<void> {
    this.busy = true;
    try {
      await api.delete(`/api/cache/videos/${encodeURIComponent(videoId)}`);
      this.status = `Cleared cache for ${videoId}.`;
      await this.load();
    } catch (err) {
      this.status = `Failed to clear: ${(err as Error).message}`;
    } finally {
      this.busy = false;
    }
  }

  /** Parent-entered video ID: YouTube IDs are 11 chars of [A-Za-z0-9_-]. */
  private static readonly VIDEO_ID_RE = /^[A-Za-z0-9_-]{11}$/;

  private onVideoIdChange(e: Event): void {
    this.videoIdInput = (e.target as HTMLInputElement).value.trim();
  }

  private async onClearVideo(e: Event): Promise<void> {
    e.preventDefault();
    const id = this.videoIdInput.trim();
    if (!CacheManager.VIDEO_ID_RE.test(id)) {
      this.status = "Enter a valid 11-character YouTube video ID.";
      return;
    }
    await this.onEvict(id);
    this.videoIdInput = "";
  }

  private openConfirm(): void {
    this.confirmOpen = true;
    queueMicrotask(() => {
      this.dialog?.show?.();
      // Focus trap: move keyboard focus to the cancel button.
      this.cancelButton?.focus();
    });
  }

  private closeConfirm(): void {
    this.confirmOpen = false;
    this.dialog?.hide?.();
  }

  private async onClearAll(): Promise<void> {
    this.busy = true;
    this.closeConfirm();
    try {
      await api.post("/api/cache/clear");
      this.status = "Entire cache cleared.";
      await this.load();
    } catch (err) {
      this.status = `Failed to clear: ${(err as Error).message}`;
    } finally {
      this.busy = false;
    }
  }

  /**
   * Renders the parallel thumbnail-cache stats as additional <dt>/<dd>
   * rows inside the main cache <dl>. Separate function so the segment
   * cache and thumbnail cache stay visually grouped without duplicating
   * the surrounding markup.
   */
  private renderThumbnailCache() {
    const tc = this.stats?.thumbnail_cache;
    if (!tc) return nothing;
    const cap = tc.max_bytes;
    const unlimited = cap <= 0;
    const pct = !unlimited && cap > 0 ? Math.min(100, (tc.total_bytes / cap) * 100) : 0;
    return html`
      <dt>Thumbnail cache</dt>
      <dd>
        ${this.fmtBytes(tc.total_bytes)} · ${tc.entry_count} thumbnails
        ${unlimited
          ? html` <span class="hint">No LRU eviction (unlimited).</span>`
          : html`
              · ${this.fmtBytes(cap)} cap
              <div
                class="progress"
                role="progressbar"
                aria-valuenow=${Math.round(pct)}
                aria-valuemin="0"
                aria-valuemax="100"
              >
                <span style="width: ${pct}%"></span>
              </div>
            `}
        <span class="hint">
          Populated by the channel backfill prefetcher and by
          <code>/api/proxy/thumbnail/&lt;id&gt;</code> on cache miss.
        </span>
      </dd>
    `;
  }

  override render() {
    if (!this.stats || !this.settings) return html`<p>Loading cache…</p>`;
    const usedPct =
      !this.stats.unlimited && this.stats.max_size_bytes > 0
        ? Math.min(100, (this.stats.total_bytes / this.stats.max_size_bytes) * 100)
        : 0;
    return html`
      <article>
        <dl>
          <dt>Cache size</dt>
          <dd>
            ${this.fmtBytes(this.stats.total_bytes)} · ${this.stats.segment_count} segments ·
            ${this.stats.video_count} videos
          </dd>
          <dt>Hit rate</dt>
          <dd>
            ${(this.stats.hit_rate * 100).toFixed(1)}% (hits: ${this.stats.hit_count}, misses:
            ${this.stats.miss_count})
          </dd>
          <dt>Limit</dt>
          <dd>
            ${this.stats.max_size_label}
            ${this.stats.unlimited
              ? html`<span class="hint">
                  No size-based (LRU) eviction will run. Videos can still be evicted by the cleanup
                  job when they're removed from every allowlist, or manually below.
                </span>`
              : html`<div
                  class="progress"
                  role="progressbar"
                  aria-valuenow=${Math.round(usedPct)}
                  aria-valuemin="0"
                  aria-valuemax="100"
                >
                  <span style="width: ${usedPct}%"></span>
                </div>`}
          </dd>
          ${this.renderThumbnailCache()}
        </dl>

        <div class="controls">
          <label>
            Max size
            <select
              ?disabled=${this.busy}
              .value=${this.settings.max_size}
              @change=${this.onMaxSize}
            >
              ${PRESETS.map(
                (p) => html`<option value=${p} ?selected=${p === this.settings!.max_size}>
                  ${p}
                </option>`,
              )}
            </select>
          </label>
          <button type="button" class="danger" ?disabled=${this.busy} @click=${this.openConfirm}>
            Clear entire cache
          </button>
        </div>

        <form class="clear-by-id" @submit=${this.onClearVideo}>
          <label for="cache-video-id">
            Clear cache for a specific video
            <span id="cache-video-id-hint" class="hint">
              Removes the DB metadata cache and on-disk segments for one YouTube video ID (11
              characters).
            </span>
          </label>
          <div class="clear-by-id-row">
            <input
              id="cache-video-id"
              type="text"
              inputmode="text"
              autocomplete="off"
              spellcheck="false"
              maxlength="11"
              placeholder="e.g. dQw4w9WgXcQ"
              .value=${this.videoIdInput}
              ?disabled=${this.busy}
              @input=${this.onVideoIdChange}
              aria-describedby="cache-video-id-hint"
            />
            <button
              type="submit"
              class="danger"
              ?disabled=${this.busy || !CacheManager.VIDEO_ID_RE.test(this.videoIdInput)}
            >
              Clear video cache
            </button>
          </div>
        </form>

        ${this.stats.top_videos.length > 0
          ? html`<table>
              <thead>
                <tr>
                  <th scope="col">Video</th>
                  <th scope="col">Segments</th>
                  <th scope="col">Size</th>
                  <th scope="col">Actions</th>
                </tr>
              </thead>
              <tbody>
                ${this.stats.top_videos.map(
                  (v) => html`<tr>
                    <td><code>${v.video_id}</code></td>
                    <td>${v.segment_count}</td>
                    <td>${this.fmtBytes(v.total_bytes)}</td>
                    <td>
                      <button
                        type="button"
                        ?disabled=${this.busy}
                        @click=${() => this.onEvict(v.video_id)}
                      >
                        Evict
                      </button>
                    </td>
                  </tr>`,
                )}
              </tbody>
            </table>`
          : html`<p>No segments cached yet.</p>`}
        <h3 class="evictions-heading">Recent evictions</h3>
        ${this.evictions.length === 0
          ? html`<p class="hint">No cache evictions recorded yet.</p>`
          : html`<table>
              <thead>
                <tr>
                  <th scope="col">When</th>
                  <th scope="col">Video</th>
                  <th scope="col">Segments</th>
                  <th scope="col">Freed</th>
                  <th scope="col">Reason</th>
                </tr>
              </thead>
              <tbody>
                ${this.evictions.map(
                  (e) => html`<tr>
                    <td>
                      <time datetime=${new Date(e.evicted_at * 1000).toISOString()}>
                        ${this.fmtTimestamp(e.evicted_at)}
                      </time>
                    </td>
                    <td><code>${e.video_id}</code></td>
                    <td>${e.segment_count}</td>
                    <td>${this.fmtBytes(e.bytes_freed)}</td>
                    <td>${this.fmtReason(e.reason)}</td>
                  </tr>`,
                )}
              </tbody>
            </table>`}

        <div class="live" role="status" aria-live="polite">${this.status}</div>

        ${this.confirmOpen
          ? html`<wa-dialog label="Clear segment cache?" open>
              <p>
                This removes every cached video segment from disk. Future playback re-downloads them
                from YouTube on demand.
              </p>
              <div class="dialog-actions">
                <button type="button" class="cancel-confirm" @click=${this.closeConfirm}>
                  Cancel
                </button>
                <button type="button" class="danger" @click=${this.onClearAll}>Clear cache</button>
              </div>
            </wa-dialog>`
          : nothing}
      </article>
    `;
  }
}

declare global {
  interface HTMLElementTagNameMap {
    "hometube-cache-manager": CacheManager;
  }
}
