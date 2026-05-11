/**
 * <hometube-bookmark-button video-id="...">
 *
 * Player chrome control: opens a small dialog with the current
 * timestamp (read from a sibling <video> element via a custom event)
 * and an optional label, then POSTs to /api/bookmarks.
 *
 * The host component (the player page) must dispatch
 * `hometube:current-time` events with `detail.seconds` so the dialog
 * pre-fills correctly. We also expose a `setCurrentTime(s: number)`
 * method for direct integration.
 *
 * Emits `hometube:bookmarks-loaded` with a list of timestamps so the
 * player can paint markers on the seek bar.
 */

import { LitElement, html, css } from "lit";
import { customElement, property, query, state } from "lit/decorators.js";

import { ApiError, api } from "../services/api.js";
import type { Bookmark } from "../types/index.js";

@customElement("hometube-bookmark-button")
export class BookmarkButton extends LitElement {
  @property({ type: String, attribute: "video-id" })
  videoId = "";

  @state() private currentTime = 0;
  @state() private label = "";
  @state() private busy = false;
  @state() private error = "";

  @query("wa-dialog") private dialog!: HTMLElement & {
    show?: () => void;
    hide?: () => void;
  };

  static styles = css`
    :host {
      display: inline-block;
    }
    button {
      padding: 0.45rem 0.9rem;
      border-radius: 999px;
      border: 1px solid var(--wa-color-surface-border, #ccc);
      background: transparent;
      color: var(--wa-color-text-normal);
      font: inherit;
      cursor: pointer;
    }
    form {
      display: grid;
      gap: 0.5rem;
    }
    label {
      display: grid;
      gap: 0.25rem;
      font-size: 0.9rem;
    }
    input {
      padding: 0.5rem;
      border: 1px solid var(--wa-color-surface-border, #ccc);
      border-radius: 0.375rem;
      background: var(--wa-color-surface-default);
      color: var(--wa-color-text-normal);
      font: inherit;
    }
    .actions {
      display: flex;
      justify-content: flex-end;
      gap: 0.5rem;
    }
    .actions button.primary {
      background: var(--wa-color-brand-fill, #2563eb);
      color: white;
    }
    .error {
      color: var(--wa-color-danger-fill, #b91c1c);
    }
    .ts {
      font-variant-numeric: tabular-nums;
      color: var(--wa-color-text-quiet);
    }
  `;

  override connectedCallback(): void {
    super.connectedCallback();
    document.addEventListener("hometube:current-time", this.onCurrentTime as EventListener);
    void this.loadBookmarks();
  }

  override disconnectedCallback(): void {
    super.disconnectedCallback();
    document.removeEventListener("hometube:current-time", this.onCurrentTime as EventListener);
  }

  override updated(changed: Map<string, unknown>): void {
    if (changed.has("videoId")) void this.loadBookmarks();
  }

  /** External setter: update the current playback time. */
  setCurrentTime(seconds: number): void {
    this.currentTime = Math.max(0, Math.floor(seconds));
  }

  private onCurrentTime = (e: Event): void => {
    const detail = (e as CustomEvent<{ seconds: number }>).detail;
    if (typeof detail?.seconds === "number") {
      this.currentTime = Math.max(0, Math.floor(detail.seconds));
    }
  };

  private async loadBookmarks(): Promise<void> {
    if (!this.videoId) return;
    try {
      const list = await api.get<Bookmark[]>(`/api/bookmarks/${encodeURIComponent(this.videoId)}`);
      this.dispatchEvent(
        new CustomEvent("hometube:bookmarks-loaded", {
          detail: { bookmarks: list },
          bubbles: true,
          composed: true,
        }),
      );
    } catch {
      // ignore
    }
  }

  private openDialog = (): void => {
    this.label = "";
    this.error = "";
    queueMicrotask(() => this.dialog?.show?.());
  };

  private close = (): void => {
    this.dialog?.hide?.();
  };

  private async onSubmit(e: Event): Promise<void> {
    e.preventDefault();
    if (!this.videoId) return;
    this.busy = true;
    this.error = "";
    try {
      await api.post("/api/bookmarks", {
        video_id: this.videoId,
        timestamp_seconds: this.currentTime,
        label: this.label.trim() || null,
      });
      await this.loadBookmarks();
      this.close();
    } catch (err) {
      this.error = err instanceof ApiError ? String(err.body) : (err as Error).message;
    } finally {
      this.busy = false;
    }
  }

  private formatTime(s: number): string {
    const h = Math.floor(s / 3600);
    const m = Math.floor((s % 3600) / 60);
    const sec = s % 60;
    if (h > 0) return `${h}:${String(m).padStart(2, "0")}:${String(sec).padStart(2, "0")}`;
    return `${m}:${String(sec).padStart(2, "0")}`;
  }

  override render() {
    return html`
      <button type="button" aria-label="Bookmark this moment" @click=${this.openDialog}>
        ★ Bookmark
      </button>

      <wa-dialog label="Save bookmark" aria-label="Save bookmark">
        <form @submit=${(e: Event) => void this.onSubmit(e)}>
          <p class="ts">
            Timestamp:
            <strong>${this.formatTime(this.currentTime)}</strong>
          </p>
          <label>
            Label (optional)
            <input
              type="text"
              placeholder="e.g. Best part!"
              .value=${this.label}
              @input=${(e: Event) => (this.label = (e.target as HTMLInputElement).value)}
            />
          </label>
          ${this.error ? html`<p class="error" role="alert">${this.error}</p>` : null}
          <div class="actions">
            <button type="button" @click=${this.close} ?disabled=${this.busy}>Cancel</button>
            <button type="submit" class="primary" ?disabled=${this.busy}>
              ${this.busy ? "Saving…" : "Save bookmark"}
            </button>
          </div>
        </form>
      </wa-dialog>
    `;
  }
}

declare global {
  interface HTMLElementTagNameMap {
    "hometube-bookmark-button": BookmarkButton;
  }
}
