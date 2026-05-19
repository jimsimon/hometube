/**
 * <hometube-change-pin-dialog>
 *
 * Modal that lets the currently signed-in parent change their own PIN.
 * PUTs to `/api/auth/pin` (which only ever updates the calling
 * account's PIN — so this is intentionally limited to the user's own
 * PIN, never another parent's).
 *
 * Toggle the `open` boolean to control visibility. Dispatches
 * `change-pin-closed` when dismissed and `change-pin-saved` after a
 * successful update.
 */

import { LitElement, html, css, nothing } from "lit";
import { customElement, property, state, query } from "lit/decorators.js";

import { ApiError, api } from "../services/api.js";

import "./error-banner.js";

@customElement("hometube-change-pin-dialog")
export class ChangePinDialog extends LitElement {
  @property({ type: Boolean, reflect: true }) open = false;

  /**
   * How long (ms) to leave the success confirmation visible before
   * auto-closing the dialog. Set to `0` to disable auto-close (e.g.
   * in tests).
   */
  @property({ type: Number, attribute: "auto-close-ms" })
  autoCloseMs = 1200;

  @state() private busy = false;
  @state() private error = "";
  @state() private saved = false;

  /** Timer handle for the post-save auto-close, so we can cancel it
   *  on disconnect (e.g. between test runs) and avoid touching a
   *  detached element. */
  private autoCloseTimer: number | undefined;

  @query("wa-dialog") private dialog!: HTMLElement & {
    show?: () => void;
    hide?: () => void;
  };
  @query('input[name="current-pin"]') private currentPinInput?: HTMLInputElement;

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
    button[disabled] {
      opacity: 0.5;
      cursor: not-allowed;
    }
    .ok {
      color: var(--wa-color-success-fill, #15803d);
      font-size: 0.9rem;
      margin: 0;
    }
  `;

  override updated(changed: Map<string, unknown>): void {
    if (!changed.has("open")) return;
    // `changed.get("open")` is the previous value. On the very first
    // render it's `undefined`, so an initial `open === false` is a
    // no-op rather than a stray `dialog.hide()` on a never-shown
    // dialog.
    const previous = changed.get("open") as boolean | undefined;
    if (this.open === previous) return;
    if (this.open) {
      this.error = "";
      this.saved = false;
      this.busy = false;
      // Clear any previously-typed PIN so it doesn't linger in the DOM
      // across reopens.
      this.renderRoot.querySelectorAll<HTMLInputElement>("input").forEach((input) => {
        input.value = "";
      });
      this.dialog?.show?.();
      // Focus is set in the dialog's `wa-after-show` handler so the
      // <wa-dialog>/<wa-dropdown> have finished their own focus
      // management before we steal it.
    } else {
      this.dialog?.hide?.();
    }
  }

  /** User-initiated dismiss. Asks the dialog to hide; `wa-after-hide`
   *  is the single source of truth that drives `onHidden`. Falls back
   *  to invoking `onHidden` directly if the dialog hasn't upgraded
   *  yet (e.g. in jsdom-based tests). */
  private requestClose = (): void => {
    if (typeof this.dialog?.hide === "function") {
      this.dialog.hide();
    } else {
      this.onHidden();
    }
  };

  override disconnectedCallback(): void {
    super.disconnectedCallback();
    window.clearTimeout(this.autoCloseTimer);
    this.autoCloseTimer = undefined;
  }

  /** Called exactly once per close cycle by `wa-after-hide`. */
  private onHidden = (): void => {
    if (!this.open) return;
    this.open = false;
    this.dispatchEvent(new CustomEvent("change-pin-closed", { bubbles: true, composed: true }));
  };

  private async onSubmit(e: Event): Promise<void> {
    e.preventDefault();
    // Prevent re-submission after a successful save (e.g. Enter key while
    // the success message is showing).
    if (this.saved || this.busy) return;
    const form = e.target as HTMLFormElement;
    const data = new FormData(form);
    const currentPin = String(data.get("current-pin") ?? "").trim();
    const pin = String(data.get("pin") ?? "").trim();
    const confirm = String(data.get("pin-confirm") ?? "").trim();

    if (!/^\d{4,6}$/.test(currentPin)) {
      this.error = "Enter your current PIN.";
      return;
    }
    if (!/^\d{4,6}$/.test(pin)) {
      this.error = "New PIN must be 4-6 digits.";
      return;
    }
    if (pin !== confirm) {
      this.error = "New PINs don't match.";
      return;
    }
    if (pin === currentPin) {
      this.error = "New PIN must be different from the current one.";
      return;
    }

    this.busy = true;
    this.error = "";
    try {
      await api.put("/api/auth/pin", { pin, current_pin: currentPin });
      this.saved = true;
      this.dispatchEvent(new CustomEvent("change-pin-saved", { bubbles: true, composed: true }));
      // Auto-close after a brief moment so the user sees the success
      // confirmation but doesn't have to click "Close" themselves.
      window.clearTimeout(this.autoCloseTimer);
      if (this.autoCloseMs > 0) {
        this.autoCloseTimer = window.setTimeout(() => {
          this.autoCloseTimer = undefined;
          if (this.saved && this.isConnected) this.requestClose();
        }, this.autoCloseMs);
      }
    } catch (err) {
      // Only 403 is reachable today (verify_pin maps to Forbidden), but
      // a stale-session 401 would also indicate "we can't accept this
      // PIN as authentication" and should redirect to login instead.
      if (err instanceof ApiError && err.status === 403) {
        this.error = "Current PIN is incorrect.";
        // Re-focus the current-PIN field so the user can correct it.
        queueMicrotask(() => {
          this.currentPinInput?.focus();
          this.currentPinInput?.select?.();
        });
      } else if (err instanceof ApiError && typeof err.body === "string" && err.body.length > 0) {
        this.error = err.body;
      } else if (err instanceof Error) {
        this.error = err.message;
      } else {
        this.error = "Something went wrong.";
      }
    } finally {
      this.busy = false;
    }
  }

  override render() {
    return html`
      <wa-dialog
        ?open=${this.open}
        label="Change your PIN"
        @wa-after-show=${() => {
          this.currentPinInput?.focus();
          this.currentPinInput?.select?.();
        }}
        @wa-after-hide=${this.onHidden}
      >
        <form @submit=${this.onSubmit} novalidate>
          <p>
            Enter your current PIN, then pick a new 4-6 digit PIN. You'll use the new PIN the next
            time you switch to your profile.
          </p>

          <label>
            Current PIN
            <input
              name="current-pin"
              type="password"
              inputmode="numeric"
              autocomplete="current-password"
              minlength="4"
              maxlength="6"
              pattern="\\d{4,6}"
              required
            />
          </label>

          <label>
            New PIN
            <input
              name="pin"
              type="password"
              inputmode="numeric"
              autocomplete="new-password"
              minlength="4"
              maxlength="6"
              pattern="\\d{4,6}"
              required
            />
          </label>

          <label>
            Confirm new PIN
            <input
              name="pin-confirm"
              type="password"
              inputmode="numeric"
              autocomplete="new-password"
              minlength="4"
              maxlength="6"
              pattern="\\d{4,6}"
              required
            />
          </label>

          ${this.error
            ? html`<hometube-error-banner .message=${this.error}></hometube-error-banner>`
            : nothing}
          ${this.saved ? html`<p class="ok" role="status">PIN updated.</p>` : nothing}

          <div class="actions">
            <button type="button" ?disabled=${this.busy} @click=${this.requestClose}>
              ${this.saved ? "Close" : "Cancel"}
            </button>
            ${!this.saved
              ? html`<button type="submit" class="primary" ?disabled=${this.busy}>
                  ${this.busy ? "Saving…" : "Save PIN"}
                </button>`
              : nothing}
          </div>
        </form>
      </wa-dialog>
    `;
  }
}

declare global {
  interface HTMLElementTagNameMap {
    "hometube-change-pin-dialog": ChangePinDialog;
  }
}
