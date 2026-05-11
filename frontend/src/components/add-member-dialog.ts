/**
 * <hometube-add-member-dialog>
 *
 * Modal form (Web Awesome `<wa-dialog>`) used by
 * `<hometube-family-manager>` to gather the role + optional display
 * name for a new family member, then `POST /api/family/members` and
 * navigate the browser to the returned `login_url`.
 *
 * Toggle visibility via the `open` attribute (or `.open` property);
 * the component dispatches `add-member-cancelled` when the user closes
 * the dialog without submitting.
 */

import { LitElement, html, css } from "lit";
import { customElement, property, state, query } from "lit/decorators.js";

import { ApiError, api } from "../services/api.js";

@customElement("hometube-add-member-dialog")
export class AddMemberDialog extends LitElement {
  @property({ type: Boolean, reflect: true }) open = false;

  @state() private busy = false;
  @state() private error = "";

  @query("wa-dialog") private dialog!: HTMLElement & {
    show?: () => void;
    hide?: () => void;
  };
  @query('input[name="display_name"]') private nameInput?: HTMLInputElement;

  static styles = css`
    :host {
      display: contents;
    }
    form {
      display: grid;
      gap: 0.75rem;
      min-width: min(20rem, 90vw);
    }
    label {
      display: grid;
      gap: 0.25rem;
      font-weight: 500;
    }
    input,
    select {
      padding: 0.5rem;
      border-radius: 0.375rem;
      border: 1px solid var(--wa-color-surface-border, #ccc);
      background: var(--wa-color-surface-default);
      color: var(--wa-color-text-normal);
      font: inherit;
    }
    .actions {
      display: flex;
      justify-content: flex-end;
      gap: 0.5rem;
      margin-top: 0.5rem;
    }
    button {
      padding: 0.5rem 1rem;
      border-radius: 0.375rem;
      border: 1px solid var(--wa-color-surface-border, #ccc);
      background: var(--wa-color-surface-default);
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
      font-size: 0.85rem;
    }
  `;

  override updated(changed: Map<string, unknown>): void {
    if (changed.has("open")) {
      if (this.open) {
        this.dialog?.show?.();
        // Focus trap: focus the first input on open.
        queueMicrotask(() => this.nameInput?.focus());
      } else {
        this.dialog?.hide?.();
      }
    }
  }

  private close(): void {
    this.open = false;
    this.dispatchEvent(
      new CustomEvent("add-member-cancelled", {
        bubbles: true,
        composed: true,
      }),
    );
  }

  private errorMessage(err: unknown): string {
    if (err instanceof ApiError) {
      if (typeof err.body === "string" && err.body.length > 0) return err.body;
      return err.message;
    }
    if (err instanceof Error) return err.message;
    return "Unknown error";
  }

  private async onSubmit(e: Event): Promise<void> {
    e.preventDefault();
    const form = e.target as HTMLFormElement;
    const data = new FormData(form);
    const role = String(data.get("role") ?? "child");
    const display_name = String(data.get("display_name") ?? "").trim();
    this.busy = true;
    this.error = "";
    try {
      const res = await api.post<{ login_url: string }>("/api/family/members", {
        role,
        display_name: display_name || undefined,
      });
      // Send the browser through the OAuth flow. The auth callback
      // will pick the role + display name back out of the signed
      // pending-member cookie and redirect to /parent/family.
      window.location.href = res.login_url;
    } catch (err) {
      this.error = this.errorMessage(err);
      this.busy = false;
    }
  }

  override render() {
    return html`
      <wa-dialog label="Add a family member" ?open=${this.open} @wa-after-hide=${this.close}>
        <form @submit=${this.onSubmit}>
          <p>
            Adding a family member starts a Google sign-in for that person. Have them sit down with
            you to pick the account.
          </p>
          <label>
            Role
            <select name="role" required>
              <option value="child" selected>Child</option>
              <option value="parent">Parent</option>
            </select>
          </label>
          <label>
            Display name
            <input
              name="display_name"
              type="text"
              autocomplete="off"
              placeholder="Optional — defaults to their Google name"
            />
          </label>
          ${this.error ? html`<p class="error" role="alert">${this.error}</p>` : null}
          <div class="actions">
            <button type="button" @click=${this.close} ?disabled=${this.busy}>Cancel</button>
            <button type="submit" class="primary" ?disabled=${this.busy}>
              ${this.busy ? "Working…" : "Continue with Google"}
            </button>
          </div>
        </form>
      </wa-dialog>
    `;
  }
}

declare global {
  interface HTMLElementTagNameMap {
    "hometube-add-member-dialog": AddMemberDialog;
  }
}
