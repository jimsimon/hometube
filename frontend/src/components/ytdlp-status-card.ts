/**
 * <hometube-ytdlp-status-card>
 *
 * Displays the yt-dlp version info from `GET /api/system/ytdlp` and a
 * "Update now" button that triggers the update job. The button announces
 * its state via an internal ARIA live region so a screen-reader user
 * hears the result without leaving the page.
 */

import { LitElement, html, css, nothing } from "lit";
import { customElement, state } from "lit/decorators.js";

import { api } from "../services/api.js";

import "./loading-spinner.js";
import "./error-banner.js";

interface YtdlpStatus {
  current_version: string | null;
  latest_known_version: string | null;
  last_checked_at: number | null;
  last_updated_at: number | null;
  binary_path: string;
}

@customElement("hometube-ytdlp-status-card")
export class YtdlpStatusCard extends LitElement {
  @state() private status: YtdlpStatus | null = null;
  @state() private error = "";
  @state() private busy = false;
  @state() private message = "";

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
    button {
      padding: 0.5rem 1rem;
      border-radius: 0.375rem;
      border: 1px solid var(--wa-color-surface-border, #ccc);
      background: var(--wa-color-brand-fill, #2563eb);
      color: white;
      font: inherit;
      cursor: pointer;
    }
    button[disabled] {
      opacity: 0.6;
      cursor: progress;
    }
    .live {
      position: absolute;
      width: 1px;
      height: 1px;
      overflow: hidden;
      clip: rect(0 0 0 0);
      white-space: nowrap;
    }
  `;

  override connectedCallback(): void {
    super.connectedCallback();
    void this.load();
  }

  private async load(): Promise<void> {
    this.error = "";
    try {
      this.status = await api.get<YtdlpStatus>("/api/system/ytdlp");
    } catch (err) {
      this.error = `Could not load yt-dlp status: ${(err as Error).message}`;
    }
  }

  private async onUpdate(): Promise<void> {
    this.busy = true;
    this.message = "Update queued…";
    try {
      await api.post("/api/system/ytdlp/update");
      this.message = "Update started. Refresh in a few seconds.";
      // Reload the status after a short delay to pick up the new
      // version + last_updated_at.
      setTimeout(() => void this.load(), 4000);
    } catch (err) {
      this.message = `Update failed: ${(err as Error).message}`;
    } finally {
      this.busy = false;
    }
  }

  private fmtDate(ts: number | null): string {
    if (!ts) return "—";
    return new Date(ts * 1000).toLocaleString();
  }

  override render() {
    if (this.error) {
      return html`<hometube-error-banner .message=${this.error}></hometube-error-banner>`;
    }
    if (!this.status)
      return html`<hometube-loading-spinner
        label="Loading yt-dlp status…"
      ></hometube-loading-spinner>`;
    return html`
      <article>
        <dl>
          <dt>Installed version</dt>
          <dd>${this.status.current_version ?? "Not yet detected"}</dd>
          <dt>Latest published</dt>
          <dd>${this.status.latest_known_version ?? "Refreshing in the background…"}</dd>
          <dt>Last checked</dt>
          <dd>${this.fmtDate(this.status.last_checked_at)}</dd>
          <dt>Last updated</dt>
          <dd>${this.fmtDate(this.status.last_updated_at)}</dd>
          <dt>Binary path</dt>
          <dd><code>${this.status.binary_path}</code></dd>
        </dl>
        <button type="button" ?disabled=${this.busy} @click=${this.onUpdate}>
          ${this.busy ? "Updating…" : "Update now"}
        </button>
        <div class="live" role="status" aria-live="polite">${this.message}</div>
        ${this.message ? html`<p>${this.message}</p>` : nothing}
      </article>
    `;
  }
}

declare global {
  interface HTMLElementTagNameMap {
    "hometube-ytdlp-status-card": YtdlpStatusCard;
  }
}
