/**
 * <hometube-ytdlp-cookies-card>
 *
 * Manages yt-dlp cookies via `GET/PUT/DELETE /api/system/ytdlp/cookies`.
 * Provides both a file picker and a textarea for supplying Netscape-format
 * cookie content. Only accessible to parent accounts (enforced server-side).
 */

import { LitElement, html, css, nothing } from "lit";
import { customElement, state } from "lit/decorators.js";

import { api } from "../services/api.js";

import "./loading-spinner.js";
import "./error-banner.js";

interface CookiesStatus {
  configured: boolean;
  line_count?: number;
}

type MessageType = "" | "success" | "error";

@customElement("hometube-ytdlp-cookies-card")
export class YtdlpCookiesCard extends LitElement {
  @state() private status: CookiesStatus | null = null;
  @state() private error = "";
  @state() private busy = false;
  @state() private message = "";
  @state() private messageType: MessageType = "";
  @state() private cookieContent = "";

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
    .status-line {
      margin: 0.5rem 0 1rem;
      font-size: 0.95rem;
    }
    .status-configured {
      color: var(--wa-color-success-text, #16a34a);
    }
    .status-not-set {
      color: var(--wa-color-neutral-text, #6b7280);
    }
    .input-section {
      margin: 1rem 0;
    }
    .input-section label {
      display: block;
      font-weight: 600;
      margin-bottom: 0.5rem;
    }
    .separator {
      text-align: center;
      margin: 1rem 0;
      color: var(--wa-color-neutral-text, #6b7280);
      font-size: 0.875rem;
    }
    textarea {
      width: 100%;
      min-height: 8rem;
      padding: 0.5rem;
      border: 1px solid var(--wa-color-surface-border, #ccc);
      border-radius: 0.375rem;
      font-family: monospace;
      font-size: 0.8125rem;
      resize: vertical;
      background: var(--wa-color-surface-alt, #f9fafb);
      color: inherit;
      box-sizing: border-box;
    }
    textarea::placeholder {
      color: var(--wa-color-neutral-text, #9ca3af);
    }
    input[type="file"] {
      font: inherit;
    }
    .actions {
      display: flex;
      gap: 0.5rem;
      margin-top: 1rem;
      flex-wrap: wrap;
    }
    button {
      padding: 0.5rem 1rem;
      border-radius: 0.375rem;
      border: 1px solid var(--wa-color-surface-border, #ccc);
      font: inherit;
      cursor: pointer;
    }
    button[disabled] {
      opacity: 0.6;
      cursor: progress;
    }
    .btn-primary {
      background: var(--wa-color-brand-fill, #2563eb);
      color: white;
      border-color: var(--wa-color-brand-fill, #2563eb);
    }
    .btn-danger {
      background: var(--wa-color-danger-fill, #dc2626);
      color: white;
      border-color: var(--wa-color-danger-fill, #dc2626);
    }
    .hint {
      margin-top: 1rem;
      font-size: 0.8125rem;
      color: var(--wa-color-neutral-text, #6b7280);
      line-height: 1.5;
    }
    .live {
      position: absolute;
      width: 1px;
      height: 1px;
      overflow: hidden;
      clip: rect(0 0 0 0);
      white-space: nowrap;
    }
    .message {
      margin-top: 0.5rem;
      font-size: 0.875rem;
    }
    .message--success {
      color: var(--wa-color-success-text, #16a34a);
    }
    .message--error {
      color: var(--wa-color-danger-text, #dc2626);
    }
  `;

  override connectedCallback(): void {
    super.connectedCallback();
    void this.load();
  }

  private async load(): Promise<void> {
    this.error = "";
    try {
      this.status = await api.get<CookiesStatus>("/api/system/ytdlp/cookies");
    } catch (err) {
      this.error = `Could not load cookie status: ${(err as Error).message}`;
    }
  }

  private onFileSelected(e: Event): void {
    const input = e.target as HTMLInputElement;
    const file = input.files?.[0];
    if (!file) return;

    const reader = new FileReader();
    reader.onload = () => {
      this.cookieContent = reader.result as string;
    };
    reader.onerror = () => {
      this.message = "Failed to read the selected file.";
      this.messageType = "error";
    };
    reader.readAsText(file);
  }

  private async onSave(): Promise<void> {
    if (!this.cookieContent.trim()) {
      this.message = "No cookie content to save. Select a file or paste content below.";
      this.messageType = "error";
      return;
    }

    this.busy = true;
    this.message = "";
    this.messageType = "";
    try {
      this.status = await api.put<CookiesStatus>("/api/system/ytdlp/cookies", {
        content: this.cookieContent,
      });
      this.message = "Cookies saved successfully.";
      this.messageType = "success";
      this.cookieContent = "";
      // Reset file input
      const fileInput = this.shadowRoot?.querySelector<HTMLInputElement>('input[type="file"]');
      if (fileInput) fileInput.value = "";
    } catch (err) {
      this.message = `Failed to save cookies: ${(err as Error).message}`;
      this.messageType = "error";
    } finally {
      this.busy = false;
    }
  }

  private async onRemove(): Promise<void> {
    if (!confirm("Remove stored cookies? yt-dlp will no longer use authenticated access.")) {
      return;
    }

    this.busy = true;
    this.message = "";
    this.messageType = "";
    try {
      this.status = await api.delete<CookiesStatus>("/api/system/ytdlp/cookies");
      this.message = "Cookies removed.";
      this.messageType = "success";
      this.cookieContent = "";
    } catch (err) {
      this.message = `Failed to remove cookies: ${(err as Error).message}`;
      this.messageType = "error";
    } finally {
      this.busy = false;
    }
  }

  override render() {
    if (this.error) {
      return html`<hometube-error-banner .message=${this.error}></hometube-error-banner>`;
    }
    if (!this.status) {
      return html`<hometube-loading-spinner
        label="Loading cookie status…"
      ></hometube-loading-spinner>`;
    }

    const statusHtml = this.status.configured
      ? html`<span class="status-configured">Configured (${this.status.line_count} lines)</span>`
      : html`<span class="status-not-set">Not configured</span>`;

    const messageClass =
      this.messageType === "error" ? "message message--error" : "message message--success";

    return html`
      <article>
        <p class="status-line"><strong>Status:</strong> ${statusHtml}</p>

        <div class="input-section">
          <label for="cookie-file">Upload a cookies.txt file</label>
          <input
            id="cookie-file"
            type="file"
            accept=".txt,text/plain"
            @change=${this.onFileSelected}
            ?disabled=${this.busy}
          />
        </div>

        <div class="separator">&mdash; or paste content below &mdash;</div>

        <div class="input-section">
          <label for="cookie-textarea">Cookie file content</label>
          <textarea
            id="cookie-textarea"
            .value=${this.cookieContent}
            @input=${(e: Event) => {
              this.cookieContent = (e.target as HTMLTextAreaElement).value;
            }}
            placeholder="# Netscape HTTP Cookie File&#10;.youtube.com&#9;TRUE&#9;/&#9;FALSE&#9;0&#9;COOKIE_NAME&#9;COOKIE_VALUE"
            ?disabled=${this.busy}
          ></textarea>
        </div>

        <div class="actions">
          <button
            type="button"
            class="btn-primary"
            ?disabled=${this.busy || !this.cookieContent.trim()}
            @click=${this.onSave}
          >
            ${this.busy ? "Saving…" : "Save cookies"}
          </button>
          ${this.status.configured
            ? html`
                <button
                  type="button"
                  class="btn-danger"
                  ?disabled=${this.busy}
                  @click=${this.onRemove}
                >
                  Remove cookies
                </button>
              `
            : nothing}
        </div>

        <div class="live" role="status" aria-live="polite">${this.message}</div>
        ${this.message ? html`<p class="${messageClass}">${this.message}</p>` : nothing}

        <p class="hint">
          Export cookies from your browser using an extension like &ldquo;Get cookies.txt
          LOCALLY&rdquo; and upload the exported file above. Cookies enable yt-dlp to access
          age-restricted or member-only content.
        </p>
      </article>
    `;
  }
}

declare global {
  interface HTMLElementTagNameMap {
    "hometube-ytdlp-cookies-card": YtdlpCookiesCard;
  }
}
