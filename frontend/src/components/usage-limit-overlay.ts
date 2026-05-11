/**
 * <hometube-usage-limit-overlay>
 *
 * Listens for the `hometube:usage-limit` CustomEvent dispatched by the
 * video player whenever the heartbeat returns a 403. Renders a
 * full-screen friendly message — "all done for today" or "outside
 * allowed hours" — and traps focus while open.
 *
 * Closing the overlay simply hides it; the player has already paused.
 */

import { LitElement, html, css } from 'lit';
import { customElement, state } from 'lit/decorators.js';

interface UsageLimitDetail {
  reason: 'limit_exceeded' | 'outside_window';
  remaining_seconds?: number;
}

@customElement('hometube-usage-limit-overlay')
export class UsageLimitOverlay extends LitElement {
  @state() private open = false;
  @state() private reason: UsageLimitDetail['reason'] | null = null;

  static styles = css`
    :host {
      display: contents;
    }
    .backdrop {
      position: fixed;
      inset: 0;
      background: rgba(0, 0, 0, 0.65);
      display: flex;
      align-items: center;
      justify-content: center;
      z-index: 1000;
    }
    .dialog {
      max-width: 28rem;
      width: calc(100% - 2rem);
      padding: 2rem;
      border-radius: 0.75rem;
      background: var(--wa-color-surface-default);
      color: var(--wa-color-text-normal);
      text-align: center;
      box-shadow: 0 1.5rem 3rem rgba(0, 0, 0, 0.3);
    }
    h2 {
      margin: 0 0 1rem;
      font-size: 1.5rem;
    }
    p {
      margin: 0 0 1.5rem;
      line-height: 1.5;
    }
    button {
      padding: 0.5rem 1.5rem;
      border-radius: 0.375rem;
      border: 1px solid var(--wa-color-surface-border, #ccc);
      background: var(--wa-color-brand-fill, #2563eb);
      color: white;
      font: inherit;
      cursor: pointer;
    }
  `;

  override connectedCallback(): void {
    super.connectedCallback();
    document.addEventListener(
      'hometube:usage-limit',
      this.onUsageLimit as EventListener,
    );
  }

  override disconnectedCallback(): void {
    super.disconnectedCallback();
    document.removeEventListener(
      'hometube:usage-limit',
      this.onUsageLimit as EventListener,
    );
  }

  private onUsageLimit = (event: Event): void => {
    const detail = (event as CustomEvent<UsageLimitDetail>).detail;
    this.reason = detail?.reason ?? 'limit_exceeded';
    this.open = true;
    // Move focus into the dialog after the next render.
    queueMicrotask(() => {
      const root = this.renderRoot as ShadowRoot;
      (root.querySelector('button') as HTMLButtonElement | null)?.focus();
    });
  };

  private close = (): void => {
    this.open = false;
  };

  override render() {
    if (!this.open) return null;
    const heading =
      this.reason === 'outside_window'
        ? "It's outside your viewing hours"
        : 'All done for today!';
    const body =
      this.reason === 'outside_window'
        ? 'Come back during your allowed hours to keep watching.'
        : "You've used up your time for today. See you tomorrow!";
    return html`
      <div
        class="backdrop"
        role="dialog"
        aria-modal="true"
        aria-labelledby="usage-overlay-title"
      >
        <div class="dialog">
          <h2 id="usage-overlay-title">${heading}</h2>
          <p>${body}</p>
          <button type="button" @click=${this.close}>OK</button>
        </div>
      </div>
    `;
  }
}

declare global {
  interface HTMLElementTagNameMap {
    'hometube-usage-limit-overlay': UsageLimitOverlay;
  }
}
