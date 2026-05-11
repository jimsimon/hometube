/**
 * <hometube-loading-spinner label="Loading…">
 *
 * Tiny, theme-aware loading indicator. Use anywhere a "Loading…" string
 * is currently inlined. Renders a CSS-only spinner with an
 * `aria-live="polite"` label so screen readers announce the state.
 */

import { LitElement, html, css } from "lit";
import { customElement, property } from "lit/decorators.js";

@customElement("hometube-loading-spinner")
export class LoadingSpinner extends LitElement {
  /** Visible + announced label. Defaults to "Loading…". */
  @property({ type: String })
  label = "Loading…";

  /** When `true`, renders inline (no centred block layout). */
  @property({ type: Boolean })
  inline = false;

  static styles = css`
    :host {
      display: inline-flex;
      align-items: center;
      gap: 0.5rem;
      color: var(--wa-color-text-quiet);
      font-size: 0.95rem;
    }
    :host([inline]) {
      display: inline-flex;
    }
    :host(:not([inline])) {
      display: flex;
      justify-content: center;
      padding: 1rem;
    }
    .spinner {
      width: 1rem;
      height: 1rem;
      border-radius: 50%;
      border: 2px solid var(--wa-color-surface-border, #ccc);
      border-top-color: var(--wa-color-brand-fill, #2563eb);
      animation: rot 0.9s linear infinite;
      flex-shrink: 0;
    }
    @keyframes rot {
      to {
        transform: rotate(360deg);
      }
    }
    @media (prefers-reduced-motion: reduce) {
      .spinner {
        animation: none;
      }
    }
  `;

  override render() {
    return html`
      <span class="spinner" aria-hidden="true"></span>
      <span role="status" aria-live="polite">${this.label}</span>
    `;
  }
}

declare global {
  interface HTMLElementTagNameMap {
    "hometube-loading-spinner": LoadingSpinner;
  }
}
