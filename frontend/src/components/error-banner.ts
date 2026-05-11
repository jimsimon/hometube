/**
 * <hometube-error-banner message="...">
 *
 * Reusable error presentation for any page that wants consistent
 * error UI. Renders a `role="alert"` banner with the message, a
 * "dismiss" button, and an optional "retry" button driven by the
 * `dismissible` / `retryable` boolean attributes.
 *
 * Components consuming this should listen for the bubbling
 * `hometube:error-retry` event when `retryable`, and for
 * `hometube:error-dismiss` when `dismissible`.
 */

import { LitElement, html, css, nothing } from 'lit';
import { customElement, property } from 'lit/decorators.js';

@customElement('hometube-error-banner')
export class ErrorBanner extends LitElement {
  /** Message text to display. Empty hides the banner. */
  @property({ type: String })
  message = '';

  /** When set, render a "Dismiss" button. */
  @property({ type: Boolean })
  dismissible = false;

  /** When set, render a "Retry" button. */
  @property({ type: Boolean })
  retryable = false;

  static styles = css`
    :host {
      display: block;
    }
    .banner {
      display: flex;
      gap: 0.75rem;
      align-items: center;
      padding: 0.75rem 1rem;
      border: 1px solid var(--wa-color-danger-fill, #b91c1c);
      border-radius: 0.5rem;
      background: var(--wa-color-danger-quiet, rgba(185, 28, 28, 0.1));
      color: var(--wa-color-danger-on-quiet, #991b1b);
    }
    .message {
      flex: 1;
      min-width: 0;
    }
    button {
      padding: 0.35rem 0.75rem;
      border-radius: 0.375rem;
      border: 1px solid currentColor;
      background: transparent;
      color: inherit;
      font: inherit;
      cursor: pointer;
    }
    button:focus-visible {
      outline: 2px solid var(--wa-color-brand-fill, #2563eb);
      outline-offset: 2px;
    }
  `;

  private onRetry = (): void => {
    this.dispatchEvent(
      new CustomEvent('hometube:error-retry', {
        bubbles: true,
        composed: true,
      }),
    );
  };

  private onDismiss = (): void => {
    this.dispatchEvent(
      new CustomEvent('hometube:error-dismiss', {
        bubbles: true,
        composed: true,
      }),
    );
    this.message = '';
  };

  override render() {
    if (!this.message) return nothing;
    return html`
      <div class="banner" role="alert">
        <span class="message">${this.message}</span>
        ${this.retryable
          ? html`<button type="button" @click=${this.onRetry}>Retry</button>`
          : nothing}
        ${this.dismissible
          ? html`<button
              type="button"
              aria-label="Dismiss error"
              @click=${this.onDismiss}
            >
              Dismiss
            </button>`
          : nothing}
      </div>
    `;
  }
}

declare global {
  interface HTMLElementTagNameMap {
    'hometube-error-banner': ErrorBanner;
  }
}
