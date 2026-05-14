/**
 * <hometube-usage-limit-overlay>
 *
 * Listens for the `hometube:usage-limit` CustomEvent dispatched by the
 * video player whenever the heartbeat returns a 403. Renders a
 * full-screen friendly message — "all done for today" or "outside
 * allowed hours" — using a <wa-dialog> for proper focus trapping and
 * accessibility.
 *
 * For `outside_window` we show the next allowed start time pulled from
 * the heartbeat response (`allowed_window`), so the message reads
 * "You can watch again at 8:00 AM".
 */

import { LitElement, html, css, nothing } from "lit";
import { customElement, state } from "lit/decorators.js";

interface AllowedWindow {
  start: string; // "HH:MM"
  end: string;
}

interface UsageLimitDetail {
  reason: "limit_exceeded" | "outside_window";
  remaining_seconds?: number;
  allowed_window?: AllowedWindow | null;
}

@customElement("hometube-usage-limit-overlay")
export class UsageLimitOverlay extends LitElement {
  @state() private open = false;
  @state() private reason: UsageLimitDetail["reason"] | null = null;
  @state() private allowedWindow: AllowedWindow | null = null;

  static styles = css`
    :host {
      display: contents;
    }
    .body {
      text-align: center;
    }
    h2 {
      margin: 0 0 1rem;
      font-size: 1.5rem;
    }
    p {
      margin: 0 0 1.5rem;
      line-height: 1.5;
    }
    .actions {
      display: flex;
      gap: 0.75rem;
      justify-content: center;
      flex-wrap: wrap;
    }
  `;

  override connectedCallback(): void {
    super.connectedCallback();
    document.addEventListener("hometube:usage-limit", this.onUsageLimit as EventListener);
  }

  override disconnectedCallback(): void {
    super.disconnectedCallback();
    document.removeEventListener("hometube:usage-limit", this.onUsageLimit as EventListener);
  }

  private onUsageLimit = (event: Event): void => {
    const detail = (event as CustomEvent<UsageLimitDetail>).detail;
    this.reason = detail?.reason ?? "limit_exceeded";
    this.allowedWindow = detail?.allowed_window ?? null;
    this.open = true;
  };

  private close = (): void => {
    this.open = false;
  };

  private formatTime(hhmm: string): string {
    const m = /^(\d{2}):(\d{2})$/.exec(hhmm);
    if (!m) return hhmm;
    const h = Number(m[1]);
    const min = m[2];
    const ampm = h < 12 ? "AM" : "PM";
    const h12 = h % 12 === 0 ? 12 : h % 12;
    return `${h12}:${min} ${ampm}`;
  }

  override render() {
    if (!this.open) return nothing;
    const heading =
      this.reason === "outside_window" ? "It's outside your viewing hours" : "All done for today!";
    let body: string;
    if (this.reason === "outside_window") {
      if (this.allowedWindow?.start) {
        body = `You can watch again at ${this.formatTime(this.allowedWindow.start)}.`;
      } else {
        body = "Come back during your allowed hours to keep watching.";
      }
    } else {
      body = "You've used up your time for today. See you tomorrow!";
    }
    return html`
      <wa-dialog open label=${heading} @wa-hide=${(e: Event) => e.preventDefault()}>
        <div class="body">
          <p>${body}</p>
          <div class="actions">
            <wa-button variant="default" href="/child/home">Back to home</wa-button>
            <wa-button variant="brand" @click=${this.close}>OK</wa-button>
          </div>
        </div>
      </wa-dialog>
    `;
  }
}

declare global {
  interface HTMLElementTagNameMap {
    "hometube-usage-limit-overlay": UsageLimitOverlay;
  }
}
