/**
 * <hometube-pin-entry-dialog>
 *
 * Modal that prompts for a parent's PIN when switching profiles. On
 * submit, POSTs to `/api/auth/switch` with `{ account_id, pin }`. On
 * success, redirects the browser to `/`. On 401 (wrong PIN) shows an
 * inline error and lets the user try again — failed attempts are not
 * locked out per the plan, but the server logs them.
 *
 * Set `account-id` + `display-name` and toggle the `open` boolean to
 * control visibility. Dispatches `pin-cancelled` when closed without
 * a successful switch.
 */

import { LitElement, html, css, nothing } from "lit";
import { customElement, property, state, query } from "lit/decorators.js";

import { ApiError, api } from "../services/api.js";

import "./error-banner.js";

@customElement("hometube-pin-entry-dialog")
export class PinEntryDialog extends LitElement {
  @property({ type: Boolean, reflect: true }) open = false;
  @property({ type: Number, attribute: "account-id" }) accountId = 0;
  @property({ type: String, attribute: "display-name" }) displayName = "";
  /** When true, the dialog explains that *any* parent's PIN is required
   *  (used when switching into a child profile). */
  @property({ type: Boolean, attribute: "require-parent-pin" }) requireParentPin = false;

  @state() private busy = false;
  @state() private error = "";

  @query("wa-dialog") private dialog!: HTMLElement & {
    show?: () => void;
    hide?: () => void;
  };
  @query('input[name="pin"]') private pinInput?: HTMLInputElement;

  static styles = css`
    :host {
      display: contents;
    }
    form {
      display: grid;
      gap: 0.75rem;
      min-width: min(18rem, 90vw);
    }
    label {
      display: grid;
      gap: 0.25rem;
      font-weight: 500;
    }
    input {
      padding: 0.5rem;
      border-radius: 0.375rem;
      border: 1px solid var(--wa-color-surface-border, #ccc);
      background: var(--wa-color-surface-default);
      color: var(--wa-color-text-normal);
      font: inherit;
      letter-spacing: 0.5em;
      text-align: center;
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
      margin: 0;
    }
  `;

  override updated(changed: Map<string, unknown>): void {
    if (changed.has("open")) {
      if (this.open) {
        this.error = "";
        this.dialog?.show?.();
        // Focus the PIN input on open. Use queueMicrotask so the dialog
        // has finished opening and can hand focus over.
        queueMicrotask(() => {
          this.pinInput?.focus();
          this.pinInput?.select?.();
        });
      } else {
        this.dialog?.hide?.();
      }
    }
  }

  /** Dialog dismissed via Escape, backdrop click, or programmatic close.
   *  Only closes the dialog and lets the host (e.g. the profile picker)
   *  decide what to do next. */
  private close(): void {
    this.open = false;
    this.dispatchEvent(new CustomEvent("pin-cancelled", { bubbles: true, composed: true }));
  }

  /** Explicit Cancel button — emits `pin-cancel-clicked` in addition to
   *  the regular `pin-cancelled` event. The host page can listen for
   *  the explicit-cancel event to navigate the user back to where they
   *  came from; we don't navigate from the dialog itself so that the
   *  dialog stays self-contained and easy to test in isolation. */
  private onCancelClick(): void {
    this.dispatchEvent(new CustomEvent("pin-cancel-clicked", { bubbles: true, composed: true }));
    this.close();
  }

  private async onSubmit(e: Event): Promise<void> {
    e.preventDefault();
    if (!this.accountId) return;
    const form = e.target as HTMLFormElement;
    const data = new FormData(form);
    const pin = String(data.get("pin") ?? "").trim();
    if (!/^\d{4,6}$/.test(pin)) {
      this.error = "PIN must be 4-6 digits.";
      return;
    }
    this.busy = true;
    this.error = "";
    try {
      await api.post("/api/auth/switch", {
        account_id: this.accountId,
        pin,
      });
      window.location.href = "/";
    } catch (err) {
      if (err instanceof ApiError && (err.status === 401 || err.status === 403)) {
        this.error = "That PIN didn't match. Try again.";
      } else if (err instanceof ApiError && typeof err.body === "string") {
        this.error = err.body;
      } else if (err instanceof Error) {
        this.error = err.message;
      } else {
        this.error = "Something went wrong.";
      }
      this.busy = false;
      // Re-focus the input so the user can retry quickly.
      this.pinInput?.focus();
      this.pinInput?.select?.();
    }
  }

  override render() {
    return html`
      <wa-dialog
        ?open=${this.open}
        label=${this.requireParentPin
          ? this.displayName
            ? `Switch to ${this.displayName}`
            : "Parent PIN required"
          : this.displayName
            ? `Enter PIN for ${this.displayName}`
            : "Enter PIN"}
        @wa-after-hide=${() => this.close()}
      >
        <form @submit=${this.onSubmit} novalidate>
          ${this.requireParentPin
            ? html`<p>
                Enter <strong>any parent's PIN</strong> to switch to
                ${this.displayName ? html`<strong>${this.displayName}</strong>` : "this profile"}.
              </p>`
            : this.displayName
              ? html`<p>Enter the PIN for <strong>${this.displayName}</strong>.</p>`
              : nothing}
          <label>
            PIN
            <input
              name="pin"
              type="password"
              inputmode="numeric"
              autocomplete="off"
              minlength="4"
              maxlength="6"
              pattern="\\d{4,6}"
              required
              autofocus
            />
          </label>
          ${this.error
            ? html`<hometube-error-banner .message=${this.error}></hometube-error-banner>`
            : nothing}
          <div class="actions">
            <button type="button" ?disabled=${this.busy} @click=${this.onCancelClick}>
              Cancel
            </button>
            <button type="submit" class="primary" ?disabled=${this.busy}>
              ${this.busy ? "Checking…" : "Continue"}
            </button>
          </div>
        </form>
      </wa-dialog>
    `;
  }
}

declare global {
  interface HTMLElementTagNameMap {
    "hometube-pin-entry-dialog": PinEntryDialog;
  }
}
