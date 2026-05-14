/**
 * <hometube-wind-down-overlay>
 *
 * Shown when the sleep timer expires (event:
 * `hometube:sleep-timer-expired`). Renders a calm, child-friendly
 * "time to stop" message with a "Back to home" button using
 * <wa-dialog> for proper focus trapping and accessibility.
 */

import { LitElement, html, css, nothing } from "lit";
import { customElement, state } from "lit/decorators.js";

@customElement("hometube-wind-down-overlay")
export class WindDownOverlay extends LitElement {
  @state() private open = false;

  static styles = css`
    :host {
      display: contents;
    }
    .body {
      text-align: center;
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
  };

  override render() {
    if (!this.open) return nothing;
    return html`
      <wa-dialog open label="Time to stop" @wa-hide=${(e: Event) => e.preventDefault()}>
        <div class="body">
          <p>Your sleep timer is up. Great work watching!</p>
          <div class="actions">
            <wa-button variant="brand" href="/child/home">Back to home</wa-button>
          </div>
        </div>
      </wa-dialog>
    `;
  }
}

declare global {
  interface HTMLElementTagNameMap {
    "hometube-wind-down-overlay": WindDownOverlay;
  }
}
