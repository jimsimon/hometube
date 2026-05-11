/**
 * <hometube-setup-pin-input>
 *
 * Step 4 of the setup wizard. Collects a 4-6 digit numeric PIN with a
 * confirmation field, then `PUT /api/auth/pin` to persist it for the
 * currently signed-in parent.
 *
 * Emits a bubbling `setup-pin-saved` CustomEvent on success.
 */

import { LitElement, html, css } from 'lit';
import { customElement, state } from 'lit/decorators.js';

import { ApiError, api } from '../services/api.js';

type Status =
  | { kind: 'idle' }
  | { kind: 'saving' }
  | { kind: 'saved' }
  | { kind: 'error'; message: string };

@customElement('hometube-setup-pin-input')
export class SetupPinInput extends LitElement {
  @state() private status: Status = { kind: 'idle' };
  @state() private localError = '';

  static styles = css`
    :host {
      display: block;
    }
    form {
      display: grid;
      gap: 1rem;
      max-width: 24rem;
    }
    label {
      display: grid;
      gap: 0.25rem;
      font-weight: 500;
    }
    input {
      padding: 0.5rem;
      border: 1px solid var(--wa-color-surface-border, #ccc);
      border-radius: 0.375rem;
      font: inherit;
      letter-spacing: 0.5em;
      text-align: center;
    }
    button {
      padding: 0.5rem 1rem;
      border-radius: 0.375rem;
      border: 0;
      background: var(--wa-color-brand-fill, #2563eb);
      color: white;
      font: inherit;
      cursor: pointer;
    }
    button[disabled] {
      opacity: 0.5;
      cursor: not-allowed;
    }
    .status.error {
      color: var(--wa-color-danger-fill, #b91c1c);
    }
    .status.ok {
      color: var(--wa-color-success-fill, #15803d);
    }
  `;

  private getValue(id: string): string {
    return (
      (this.renderRoot as ShadowRoot).getElementById(id) as HTMLInputElement | null
    )?.value.trim() ?? '';
  }

  private validate(pin: string, confirm: string): string | null {
    if (!/^\d{4,6}$/.test(pin)) return 'PIN must be 4-6 digits.';
    if (pin !== confirm) return "PINs don't match.";
    return null;
  }

  private async onSubmit(e: Event): Promise<void> {
    e.preventDefault();
    const pin = this.getValue('pin');
    const confirm = this.getValue('pin-confirm');
    const validation = this.validate(pin, confirm);
    if (validation) {
      this.localError = validation;
      this.status = { kind: 'idle' };
      return;
    }
    this.localError = '';
    this.status = { kind: 'saving' };
    try {
      await api.put('/api/auth/pin', { pin });
      this.status = { kind: 'saved' };
      this.dispatchEvent(
        new CustomEvent('setup-pin-saved', { bubbles: true, composed: true }),
      );
    } catch (err) {
      this.status = {
        kind: 'error',
        message: this.errorMessage(err),
      };
    }
  }

  private errorMessage(err: unknown): string {
    if (err instanceof ApiError) {
      if (typeof err.body === 'string' && err.body.length > 0) return err.body;
      return err.message;
    }
    if (err instanceof Error) return err.message;
    return 'Unknown error';
  }

  override render() {
    const busy = this.status.kind === 'saving';
    return html`
      <form @submit=${this.onSubmit} novalidate>
        <p>
          Pick a 4-6 digit PIN. You'll enter it any time you switch to
          your parent profile.
        </p>

        <label for="pin">
          PIN
          <input
            id="pin"
            type="password"
            inputmode="numeric"
            autocomplete="new-password"
            minlength="4"
            maxlength="6"
            pattern="\\d{4,6}"
            required
          />
        </label>

        <label for="pin-confirm">
          Confirm PIN
          <input
            id="pin-confirm"
            type="password"
            inputmode="numeric"
            autocomplete="new-password"
            minlength="4"
            maxlength="6"
            pattern="\\d{4,6}"
            required
          />
        </label>

        <button type="submit" ?disabled=${busy}>
          ${busy ? 'Saving…' : 'Save PIN'}
        </button>

        ${this.localError
          ? html`<p class="status error" role="alert">${this.localError}</p>`
          : null}
        ${this.status.kind === 'saved'
          ? html`<p class="status ok" role="status">PIN saved.</p>`
          : null}
        ${this.status.kind === 'error'
          ? html`<p class="status error" role="alert">
              ${this.status.message}
            </p>`
          : null}
      </form>
    `;
  }
}

declare global {
  interface HTMLElementTagNameMap {
    'hometube-setup-pin-input': SetupPinInput;
  }
}
