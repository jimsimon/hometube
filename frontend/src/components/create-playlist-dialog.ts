/**
 * <hometube-create-playlist-dialog>
 *
 * Modal form for creating a new playlist. Wraps Web Awesome's
 * <wa-dialog>. Emits `hometube:playlist-created` (bubbling) on
 * success so list components can refresh themselves.
 */

import { LitElement, html, css } from "lit";
import { customElement, query, state } from "lit/decorators.js";

import { ApiError, api } from "../services/api.js";
import type { PlaylistSummary } from "../types/index.js";

import "./error-banner.js";

@customElement("hometube-create-playlist-dialog")
export class CreatePlaylistDialog extends LitElement {
  @state() private titleValue = "";
  @state() private description = "";
  @state() private busy = false;
  @state() private error = "";

  @query("wa-dialog") private dialog!: HTMLElement & {
    open?: boolean;
    show?: () => void;
    hide?: () => void;
  };

  static styles = css`
    :host {
      display: contents;
    }
    form {
      display: grid;
      gap: 0.75rem;
    }
    label {
      display: grid;
      gap: 0.25rem;
      font-size: 0.9rem;
    }
    input,
    textarea {
      padding: 0.5rem;
      border: 1px solid var(--wa-color-surface-border, #ccc);
      border-radius: 0.375rem;
      background: var(--wa-color-surface-default);
      color: var(--wa-color-text-normal);
      font: inherit;
    }
    textarea {
      min-height: 4rem;
      resize: vertical;
    }
    .actions {
      display: flex;
      gap: 0.5rem;
      justify-content: flex-end;
      margin-top: 0.5rem;
    }
    button {
      padding: 0.45rem 0.9rem;
      border-radius: 0.375rem;
      border: 1px solid var(--wa-color-surface-border);
      font: inherit;
      cursor: pointer;
    }
    button[type="submit"] {
      background: var(--wa-color-brand-fill, #2563eb);
      color: white;
    }
    button.secondary {
      background: transparent;
      color: var(--wa-color-text-normal);
    }
    .error {
      color: var(--wa-color-danger-fill, #b91c1c);
    }
  `;

  /** Public method: open the dialog and reset state. */
  open(): void {
    this.titleValue = "";
    this.description = "";
    this.error = "";
    queueMicrotask(() => this.dialog?.show?.());
  }

  private close = (): void => {
    this.dialog?.hide?.();
  };

  private async onSubmit(e: Event): Promise<void> {
    e.preventDefault();
    if (!this.titleValue.trim()) {
      this.error = "Title is required.";
      return;
    }
    this.busy = true;
    this.error = "";
    try {
      await api.post<PlaylistSummary>("/api/playlists", {
        title: this.titleValue.trim(),
        description: this.description.trim() || null,
      });
      this.dispatchEvent(
        new CustomEvent("hometube:playlist-created", {
          bubbles: true,
          composed: true,
        }),
      );
      this.close();
    } catch (err) {
      this.error = err instanceof ApiError ? String(err.body) : (err as Error).message;
    } finally {
      this.busy = false;
    }
  }

  override render() {
    return html`
      <wa-dialog label="Create playlist" aria-label="Create playlist">
        <form @submit=${(e: Event) => void this.onSubmit(e)}>
          <label>
            Title
            <input
              type="text"
              required
              .value=${this.titleValue}
              @input=${(e: Event) => (this.titleValue = (e.target as HTMLInputElement).value)}
            />
          </label>
          <label>
            Description (optional)
            <textarea
              .value=${this.description}
              @input=${(e: Event) => (this.description = (e.target as HTMLTextAreaElement).value)}
            ></textarea>
          </label>
          ${this.error
            ? html`<hometube-error-banner .message=${this.error}></hometube-error-banner>`
            : null}
          <div class="actions">
            <button type="button" class="secondary" ?disabled=${this.busy} @click=${this.close}>
              Cancel
            </button>
            <button type="submit" ?disabled=${this.busy}>
              ${this.busy ? "Creating…" : "Create"}
            </button>
          </div>
        </form>
      </wa-dialog>
    `;
  }
}

declare global {
  interface HTMLElementTagNameMap {
    "hometube-create-playlist-dialog": CreatePlaylistDialog;
  }
}
