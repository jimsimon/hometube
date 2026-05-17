/**
 * <hometube-family-member-card>
 *
 * Renders a single row in the family-management list: avatar, name,
 * role badge, and last-login timestamp, plus action buttons:
 *
 *   - Edit   → opens an inline rename / role-change form
 *   - Remove → confirmation dialog then DELETE
 *
 * The card never mutates its own `member` prop directly; on success it
 * dispatches a bubbling `family-changed` event so the parent
 * `<hometube-family-manager>` re-fetches the list.
 */

import { LitElement, html, css, nothing } from "lit";
import { customElement, property, state, query } from "lit/decorators.js";

import { ApiError, api } from "../services/api.js";

export interface FamilyMember {
  id: number;
  display_name: string;
  avatar_url: string | null;
  account_type: "parent" | "child";
  has_pin: boolean;
  created_at: number;
  last_login_at: number | null;
}

@customElement("hometube-family-member-card")
export class FamilyMemberCard extends LitElement {
  @property({ attribute: false }) member: FamilyMember | null = null;

  @state() private editing = false;
  @state() private removing = false;
  @state() private busy = false;
  @state() private status = "";

  @query("wa-dialog.confirm-remove") private removeDialog?: HTMLElement & {
    show?: () => void;
    hide?: () => void;
  };

  static styles = css`
    :host {
      display: block;
      margin-bottom: 0.75rem;
    }
    article {
      display: grid;
      grid-template-columns: auto 1fr auto;
      gap: 1rem;
      align-items: center;
      padding: 1rem;
      border: 1px solid var(--wa-color-surface-border);
      border-radius: 0.5rem;
      background: var(--wa-color-surface-default);
    }
    .avatar {
      width: 3rem;
      height: 3rem;
      border-radius: 50%;
      background: var(--wa-color-surface-raised);
      display: grid;
      place-items: center;
      font-weight: 700;
      overflow: hidden;
    }
    .avatar img {
      width: 100%;
      height: 100%;
      object-fit: cover;
    }
    .info {
      display: grid;
      gap: 0.15rem;
      min-width: 0;
    }
    .name {
      font-weight: 600;
      overflow: hidden;
      text-overflow: ellipsis;
      white-space: nowrap;
    }
    .badges {
      display: flex;
      gap: 0.5rem;
      flex-wrap: wrap;
      margin-top: 0.25rem;
    }
    .badge {
      font-size: 0.75rem;
      padding: 0.1rem 0.5rem;
      border-radius: 999px;
      background: var(--wa-color-surface-raised);
    }
    .badge.parent {
      background: var(--wa-color-brand-quiet, rgba(37, 99, 235, 0.15));
      color: var(--wa-color-brand-on-quiet, #1d4ed8);
    }
    .badge.child {
      background: var(--wa-color-success-quiet, rgba(34, 197, 94, 0.15));
      color: var(--wa-color-success-on-quiet, #166534);
    }
    .badge.warn {
      background: var(--wa-color-warning-quiet, rgba(245, 158, 11, 0.18));
      color: var(--wa-color-warning-on-quiet, #92400e);
    }
    .actions {
      display: flex;
      flex-wrap: wrap;
      gap: 0.5rem;
      align-items: center;
    }
    button {
      padding: 0.4rem 0.75rem;
      border-radius: 0.375rem;
      border: 1px solid var(--wa-color-surface-border, #ccc);
      background: var(--wa-color-surface-default);
      color: var(--wa-color-text-normal);
      font: inherit;
      cursor: pointer;
    }
    button.danger {
      background: var(--wa-color-danger-fill, #b91c1c);
      color: white;
      border-color: transparent;
    }
    .edit-form {
      display: grid;
      gap: 0.5rem;
      margin-top: 0.75rem;
      grid-column: 1 / -1;
    }
    .edit-form .row {
      display: flex;
      flex-wrap: wrap;
      gap: 0.5rem;
      align-items: center;
    }
    input,
    select {
      padding: 0.4rem 0.5rem;
      border-radius: 0.375rem;
      border: 1px solid var(--wa-color-surface-border, #ccc);
      background: var(--wa-color-surface-default);
      color: var(--wa-color-text-normal);
      font: inherit;
    }
    .live {
      position: absolute;
      width: 1px;
      height: 1px;
      overflow: hidden;
      clip: rect(0 0 0 0);
      white-space: nowrap;
    }
    .dialog-actions {
      display: flex;
      gap: 0.5rem;
      justify-content: flex-end;
      margin-top: 1rem;
    }
  `;

  private fmtDate(ts: number | null): string {
    if (!ts) return "never";
    return new Date(ts * 1000).toLocaleString();
  }

  private initials(name: string): string {
    const parts = name.trim().split(/\s+/);
    return parts
      .slice(0, 2)
      .map((p) => p[0]?.toUpperCase() ?? "")
      .join("");
  }

  private dispatchChanged(): void {
    this.dispatchEvent(new CustomEvent("family-changed", { bubbles: true, composed: true }));
  }

  private errorMessage(err: unknown): string {
    if (err instanceof ApiError) {
      if (typeof err.body === "string" && err.body.length > 0) return err.body;
      return err.message;
    }
    if (err instanceof Error) return err.message;
    return "Unknown error";
  }

  private async onSaveEdit(e: Event): Promise<void> {
    e.preventDefault();
    if (!this.member) return;
    const form = e.target as HTMLFormElement;
    const data = new FormData(form);
    const display_name = String(data.get("display_name") ?? "").trim();
    const role = String(data.get("role") ?? "");

    this.busy = true;
    try {
      await api.put(`/api/family/members/${this.member.id}`, {
        display_name: display_name || undefined,
        role: role || undefined,
      });
      this.status = "Saved.";
      this.editing = false;
      this.dispatchChanged();
    } catch (err) {
      this.status = `Save failed: ${this.errorMessage(err)}`;
    } finally {
      this.busy = false;
    }
  }

  private openRemove(): void {
    this.removing = true;
    queueMicrotask(() => this.removeDialog?.show?.());
  }

  private closeRemove(): void {
    this.removing = false;
    this.removeDialog?.hide?.();
  }

  private async onConfirmRemove(): Promise<void> {
    if (!this.member) return;
    this.busy = true;
    try {
      await api.delete(`/api/family/members/${this.member.id}`);
      this.closeRemove();
      this.dispatchChanged();
    } catch (err) {
      this.status = `Remove failed: ${this.errorMessage(err)}`;
      this.busy = false;
    }
  }

  override render() {
    const m = this.member;
    if (!m) return nothing;
    return html`
      <article aria-labelledby="member-${m.id}-name">
        <div class="avatar" aria-hidden="true">
          ${m.avatar_url
            ? html`<img src=${m.avatar_url} alt="" />`
            : html`${this.initials(m.display_name)}`}
        </div>
        <div class="info">
          <span class="name" id="member-${m.id}-name">${m.display_name}</span>
          <div class="badges">
            <span class=${`badge ${m.account_type}`}>${m.account_type}</span>
            ${m.account_type === "parent" && !m.has_pin
              ? html`<span class="badge warn" title="Set PIN required">Set PIN required</span>`
              : nothing}
            <span class="badge">Last login: ${this.fmtDate(m.last_login_at)}</span>
          </div>
        </div>
        <div class="actions">
          <button
            type="button"
            ?disabled=${this.busy}
            @click=${() => (this.editing = !this.editing)}
          >
            ${this.editing ? "Cancel" : "Edit"}
          </button>
          <button type="button" class="danger" ?disabled=${this.busy} @click=${this.openRemove}>
            Remove
          </button>
        </div>

        ${this.editing
          ? html`<form class="edit-form" @submit=${this.onSaveEdit}>
              <div class="row">
                <label>
                  Name
                  <input name="display_name" type="text" .value=${m.display_name} required />
                </label>
                <label>
                  Role
                  <select name="role" .value=${m.account_type}>
                    <option value="parent" ?selected=${m.account_type === "parent"}>Parent</option>
                    <option value="child" ?selected=${m.account_type === "child"}>Child</option>
                  </select>
                </label>
              </div>
              <div class="row">
                <button type="submit" ?disabled=${this.busy}>Save</button>
              </div>
            </form>`
          : nothing}

        <div class="live" role="status" aria-live="polite">${this.status}</div>

        ${this.removing
          ? html`<wa-dialog class="confirm-remove" label=${`Remove ${m.display_name}?`} open>
              <p>
                Removing ${m.display_name} also signs them out everywhere and deletes their HomeTube
                data.
              </p>
              <div class="dialog-actions">
                <button type="button" @click=${this.closeRemove}>Cancel</button>
                <button
                  type="button"
                  class="danger"
                  ?disabled=${this.busy}
                  @click=${this.onConfirmRemove}
                >
                  Remove
                </button>
              </div>
            </wa-dialog>`
          : nothing}
      </article>
    `;
  }
}

declare global {
  interface HTMLElementTagNameMap {
    "hometube-family-member-card": FamilyMemberCard;
  }
}
