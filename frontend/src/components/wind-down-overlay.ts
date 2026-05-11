/**
 * <hometube-wind-down-overlay>
 *
 * Shown when the sleep timer expires (event:
 * `hometube:sleep-timer-expired`). Renders a calm, child-friendly
 * "time to stop" message with a "Back to home" button. Traps focus
 * inside the dialog while open.
 */

import { LitElement, html, css } from "lit";
import { customElement, state } from "lit/decorators.js";

@customElement("hometube-wind-down-overlay")
export class WindDownOverlay extends LitElement {
  @state() private open = false;

  static styles = css`
    :host {
      display: contents;
    }
    .backdrop {
      position: fixed;
      inset: 0;
      background: rgba(0, 0, 0, 0.7);
      display: flex;
      align-items: center;
      justify-content: center;
      z-index: 1100;
    }
    .dialog {
      max-width: 28rem;
      width: calc(100% - 2rem);
      padding: 2rem;
      border-radius: 0.75rem;
      background: var(--wa-color-surface-default);
      color: var(--wa-color-text-normal);
      text-align: center;
      box-shadow: 0 1.5rem 3rem rgba(0, 0, 0, 0.4);
    }
    h2 {
      margin: 0 0 1rem;
      font-size: 1.5rem;
    }
    p {
      line-height: 1.5;
      margin: 0 0 1.5rem;
    }
    .actions {
      display: flex;
      gap: 0.5rem;
      justify-content: center;
    }
    button,
    a.button {
      display: inline-flex;
      align-items: center;
      padding: 0.55rem 1.4rem;
      border-radius: 0.5rem;
      border: 1px solid var(--wa-color-surface-border);
      background: var(--wa-color-brand-fill, #2563eb);
      color: white;
      font: inherit;
      text-decoration: none;
      cursor: pointer;
    }
  `;

  override connectedCallback(): void {
    super.connectedCallback();
    document.addEventListener("hometube:sleep-timer-expired", this.onExpired as EventListener);
  }

  override disconnectedCallback(): void {
    super.disconnectedCallback();
    document.removeEventListener("hometube:sleep-timer-expired", this.onExpired as EventListener);
  }

  private onExpired = (): void => {
    this.open = true;
    queueMicrotask(() => {
      const root = this.renderRoot as ShadowRoot;
      (root.querySelector("a.button") as HTMLAnchorElement | null)?.focus();
    });
  };

  override render() {
    if (!this.open) return null;
    return html`
      <div class="backdrop" role="dialog" aria-modal="true" aria-labelledby="wind-down-title">
        <div class="dialog">
          <h2 id="wind-down-title">Time to stop</h2>
          <p>Your sleep timer is up. Great work watching!</p>
          <div class="actions">
            <a class="button" href="/child/home">Back to home</a>
          </div>
        </div>
      </div>
    `;
  }
}

declare global {
  interface HTMLElementTagNameMap {
    "hometube-wind-down-overlay": WindDownOverlay;
  }
}
