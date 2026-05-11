/**
 * <hometube-offline-downloads-list>
 *
 * Renders the locally-stored offline downloads (Cache API + manifest in
 * localStorage). Used by the `/child/downloads` page. Each row offers
 * a "Watch" link and a "Delete" button that purges the cached
 * `Response` plus the manifest entry.
 *
 * Storage usage is reported via `navigator.storage.estimate()` so kids
 * (and parents looking over their shoulder) know how much room is left.
 */

import { LitElement, css, html, nothing } from "lit";
import { customElement, state } from "lit/decorators.js";

import {
  type OfflineEntry,
  deleteOfflineVideo,
  getStorageEstimate,
  listOfflineVideos,
} from "../services/offline.js";

import "./video-card.js";

function formatBytes(n: number | null): string {
  if (n == null) return "";
  if (n < 1024) return `${n} B`;
  if (n < 1024 * 1024) return `${(n / 1024).toFixed(1)} KB`;
  if (n < 1024 * 1024 * 1024) return `${(n / 1024 / 1024).toFixed(1)} MB`;
  return `${(n / 1024 / 1024 / 1024).toFixed(2)} GB`;
}

@customElement("hometube-offline-downloads-list")
export class OfflineDownloadsList extends LitElement {
  @state() private entries: OfflineEntry[] = [];
  @state() private storage: {
    usage: number;
    quota: number;
    percentUsed: number;
  } | null = null;

  static styles = css`
    :host {
      display: block;
    }
    .summary {
      margin: 0 0 1rem;
      padding: 0.75rem 1rem;
      border: 1px solid var(--wa-color-surface-border);
      border-radius: 0.5rem;
      background: var(--wa-color-surface-default);
    }
    .summary p {
      margin: 0.25rem 0;
    }
    .empty {
      padding: 2rem;
      text-align: center;
      color: var(--wa-color-text-quiet);
    }
    ul {
      list-style: none;
      padding: 0;
      margin: 0;
      display: grid;
      gap: 1rem;
      grid-template-columns: repeat(auto-fill, minmax(16rem, 1fr));
    }
    li {
      display: grid;
      gap: 0.5rem;
    }
    .row-actions {
      display: flex;
      gap: 0.5rem;
      align-items: center;
      font-size: 0.85rem;
      color: var(--wa-color-text-quiet);
    }
    button {
      padding: 0.25rem 0.5rem;
      border-radius: 0.25rem;
      border: 1px solid var(--wa-color-surface-border);
      background: transparent;
      color: var(--wa-color-text-normal);
      font: inherit;
      cursor: pointer;
    }
    button:hover {
      background: var(--wa-color-surface-raised);
    }
  `;

  override connectedCallback(): void {
    super.connectedCallback();
    void this.refresh();
  }

  private async refresh(): Promise<void> {
    this.entries = listOfflineVideos();
    this.storage = await getStorageEstimate();
  }

  private onDelete = async (e: OfflineEntry): Promise<void> => {
    if (!confirm(`Delete "${e.title ?? e.videoId}" from this device?`)) {
      return;
    }
    await deleteOfflineVideo(e.videoId, e.quality);
    await this.refresh();
  };

  override render() {
    return html`
      ${this.storage
        ? html`<div class="summary" role="status">
            <p>
              <strong>Storage:</strong>
              ${formatBytes(this.storage.usage)} used of ${formatBytes(this.storage.quota)}
              (${this.storage.percentUsed.toFixed(1)}%)
            </p>
            <p><strong>Downloads:</strong> ${this.entries.length}</p>
          </div>`
        : nothing}
      ${this.entries.length === 0
        ? html`<p class="empty">
            No downloads yet. Tap the "Download" button on a video to save it for offline.
          </p>`
        : html`<ul aria-label="Downloaded videos">
            ${this.entries.map(
              (e) => html`<li>
                <hometube-video-card
                  video-id=${e.videoId}
                  .title=${e.title ?? e.videoId}
                  .thumbnailUrl=${e.thumbnailUrl}
                  .channelTitle=${e.channelTitle}
                  .duration=${e.durationSeconds}
                ></hometube-video-card>
                <div class="row-actions">
                  <span>${e.quality} · ${formatBytes(e.sizeBytes)}</span>
                  <button
                    type="button"
                    @click=${() => this.onDelete(e)}
                    aria-label="Delete ${e.title ?? e.videoId}"
                  >
                    Delete
                  </button>
                </div>
              </li>`,
            )}
          </ul>`}
    `;
  }
}

declare global {
  interface HTMLElementTagNameMap {
    "hometube-offline-downloads-list": OfflineDownloadsList;
  }
}
