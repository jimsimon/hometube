/**
 * <hometube-family-playlist-form>
 *
 * Inline form used both for creating new family playlists and editing
 * existing ones. Renders a `<wa-dialog>` with title/description fields
 * and a multi-select of child accounts.
 *
 * The host calls `open(detail?)` to show the dialog. On submission a
 * `hometube:family-playlist-saved` event bubbles up so the parent
 * (manager / detail) can refresh.
 */

import { LitElement, html, css, nothing } from "lit";
import { customElement, query, state } from "lit/decorators.js";

import { ApiError, api } from "../services/api.js";
import type { AccountSummary, FamilyPlaylistDetail } from "../types/index.js";

@customElement("hometube-family-playlist-form")
export class FamilyPlaylistForm extends LitElement {
  @state() private existing: FamilyPlaylistDetail | null = null;
  @state() private titleValue = "";
  @state() private description = "";
  @state() private childIds: number[] = [];
  @state() private childrenList: AccountSummary[] = [];
  @state() private error = "";
  @state() private saving = false;

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
      font-weight: 600;
      font-size: 0.95rem;
    }
    input[type="text"],
    textarea {
      width: 100%;
      padding: 0.5rem;
      border: 1px solid var(--wa-color-surface-border, #ccc);
      border-radius: 0.375rem;
      background: var(--wa-color-surface-default);
      color: var(--wa-color-text-normal);
      font: inherit;
    }
    .members {
      display: grid;
      gap: 0.25rem;
    }
    .members .row {
      display: flex;
      align-items: center;
      gap: 0.5rem;
    }
    .actions {
      display: flex;
      gap: 0.5rem;
      justify-content: flex-end;
    }
    button {
      padding: 0.5rem 1rem;
      border-radius: 0.375rem;
      border: 1px solid var(--wa-color-surface-border, #ccc);
      background: transparent;
      color: var(--wa-color-text-normal);
      font: inherit;
      cursor: pointer;
    }
    button.primary {
      background: var(--wa-color-brand-fill, #2563eb);
      color: white;
      border-color: transparent;
    }
    .error {
      color: var(--wa-color-danger-fill, #b91c1c);
      font-size: 0.95rem;
    }
  `;

  override connectedCallback(): void {
    super.connectedCallback();
    void this.loadChildren();
  }

  /** Open in create mode (no existing) or edit mode (existing). */
  open(existing?: FamilyPlaylistDetail): void {
    this.existing = existing ?? null;
    this.titleValue = existing?.title ?? "";
    this.description = existing?.description ?? "";
    this.childIds = existing ? [...existing.child_ids] : [];
    this.error = "";
    this.dialog?.show?.();
  }

  private async loadChildren(): Promise<void> {
    try {
      this.childrenList = await api.get<AccountSummary[]>("/api/accounts?type=child");
    } catch {
      // ignore: the form still works for the title/description path.
    }
  }

  private toggleChild(id: number, checked: boolean): void {
    if (checked) {
      if (!this.childIds.includes(id)) {
        this.childIds = [...this.childIds, id];
      }
    } else {
      this.childIds = this.childIds.filter((c) => c !== id);
    }
  }

  private onSubmit = async (e: Event): Promise<void> => {
    e.preventDefault();
    if (!this.titleValue.trim()) {
      this.error = "Title is required";
      return;
    }
    this.saving = true;
    this.error = "";
    try {
      let saved: FamilyPlaylistDetail;
      if (this.existing) {
        saved = await api.put<FamilyPlaylistDetail>(`/api/family-playlists/${this.existing.id}`, {
          title: this.titleValue,
          description: this.description,
          child_ids: this.childIds,
        });
      } else {
        saved = await api.post<FamilyPlaylistDetail>("/api/family-playlists", {
          title: this.titleValue,
          description: this.description,
          child_ids: this.childIds,
        });
      }
      this.dispatchEvent(
        new CustomEvent("hometube:family-playlist-saved", {
          detail: saved,
          bubbles: true,
          composed: true,
        }),
      );
      this.dialog?.hide?.();
    } catch (err) {
      this.error = err instanceof ApiError ? String(err.body) : (err as Error).message;
    } finally {
      this.saving = false;
    }
  };

  override render() {
    const dialogLabel = this.existing ? "Edit family playlist" : "Create family playlist";
    return html`
      <wa-dialog label=${dialogLabel} aria-label=${dialogLabel}>
        <form @submit=${this.onSubmit}>
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
            Description
            <textarea
              rows="3"
              .value=${this.description}
              @input=${(e: Event) => (this.description = (e.target as HTMLTextAreaElement).value)}
            ></textarea>
          </label>
          <fieldset class="members">
            <legend>Children with access</legend>
            ${this.childrenList.length === 0
              ? html`<span class="error">No child accounts found.</span>`
              : this.childrenList.map(
                  (c) => html`
                    <label class="row">
                      <input
                        type="checkbox"
                        .checked=${this.childIds.includes(c.id)}
                        @change=${(e: Event) =>
                          this.toggleChild(c.id, (e.target as HTMLInputElement).checked)}
                      />
                      ${c.display_name}
                    </label>
                  `,
                )}
          </fieldset>
          ${this.error ? html`<p class="error" role="alert">${this.error}</p>` : nothing}
          <div class="actions">
            <button type="button" @click=${() => this.dialog?.hide?.()} ?disabled=${this.saving}>
              Cancel
            </button>
            <button class="primary" type="submit" ?disabled=${this.saving}>
              ${this.saving ? "Saving…" : "Save"}
            </button>
          </div>
        </form>
      </wa-dialog>
    `;
  }
}

declare global {
  interface HTMLElementTagNameMap {
    "hometube-family-playlist-form": FamilyPlaylistForm;
  }
}
