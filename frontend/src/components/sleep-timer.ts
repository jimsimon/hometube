/**
 * <hometube-sleep-timer>
 *
 * Dropdown button + active-timer indicator. Lets the child set a sleep
 * timer ("Stop after this video", "Stop in 15/30/45/60 minutes") or
 * cancel an active one. The current state is fetched from
 * /api/timer; setting a new timer POSTs and cancelling DELETEs.
 *
 * When the timer expires, dispatches `hometube:sleep-timer-expired`
 * (bubbling) so the player can react. Countdown updates use a 1Hz
 * setInterval that's cleared when the dropdown is unmounted or
 * cancelled.
 *
 * The countdown text is wrapped in an `aria-live="polite"` region so
 * screen-readers get periodic updates without taking focus.
 */

import { LitElement, html, css } from "lit";
import { customElement, query, state } from "lit/decorators.js";

import { ApiError, api } from "../services/api.js";
import type { SleepTimerRow } from "../types/index.js";

const MINUTE_OPTIONS = [15, 30, 45, 60];

@customElement("hometube-sleep-timer")
export class SleepTimer extends LitElement {
  @state() private timer: SleepTimerRow | null = null;
  @state() private now = Math.floor(Date.now() / 1000);
  @state() private busy = false;
  @state() private error = "";

  @query("wa-dropdown") private dropdown!: HTMLElement & {
    show?: () => void;
    hide?: () => void;
  };

  private tickHandle: number | null = null;

  static styles = css`
    :host {
      display: inline-block;
    }
    button.trigger {
      padding: 0.45rem 0.9rem;
      border-radius: 999px;
      border: 1px solid var(--wa-color-surface-border, #ccc);
      background: transparent;
      color: var(--wa-color-text-normal);
      font: inherit;
      cursor: pointer;
    }
    button.trigger.active {
      background: var(--wa-color-warning-quiet, rgba(217, 119, 6, 0.15));
      color: var(--wa-color-warning-on-quiet, #92400e);
    }
    .menu {
      display: grid;
      gap: 0.25rem;
      padding: 0.5rem;
      min-width: 14rem;
      background: var(--wa-color-surface-default);
      border: 1px solid var(--wa-color-surface-border);
      border-radius: 0.5rem;
    }
    .menu button {
      text-align: left;
      padding: 0.5rem 0.75rem;
      border-radius: 0.375rem;
      border: none;
      background: transparent;
      color: inherit;
      font: inherit;
      cursor: pointer;
    }
    .menu button:hover,
    .menu button:focus-visible {
      background: var(--wa-color-surface-raised);
      outline: none;
    }
    .menu button.cancel {
      color: var(--wa-color-danger-fill, #b91c1c);
    }
    .countdown {
      font-variant-numeric: tabular-nums;
    }
    .error {
      color: var(--wa-color-danger-fill, #b91c1c);
      font-size: 0.85rem;
    }
  `;

  override connectedCallback(): void {
    super.connectedCallback();
    void this.refresh();
  }

  override disconnectedCallback(): void {
    super.disconnectedCallback();
    this.stopTick();
  }

  private async refresh(): Promise<void> {
    try {
      this.timer = await api.get<SleepTimerRow | null>("/api/timer");
      if (this.timer?.timer_type === "minutes" && this.timer.expires_at != null) {
        this.startTick();
      } else {
        this.stopTick();
      }
    } catch {
      this.timer = null;
    }
  }

  private startTick(): void {
    this.stopTick();
    this.tickHandle = window.setInterval(() => {
      this.now = Math.floor(Date.now() / 1000);
      if (this.timer?.expires_at != null && this.now >= this.timer.expires_at) {
        this.fireExpired();
        this.stopTick();
        // Clear server-side state too.
        void api.delete("/api/timer").catch(() => {});
        this.timer = null;
      }
    }, 1_000);
  }

  private stopTick(): void {
    if (this.tickHandle != null) {
      window.clearInterval(this.tickHandle);
      this.tickHandle = null;
    }
  }

  private fireExpired(): void {
    this.dispatchEvent(
      new CustomEvent("hometube:sleep-timer-expired", {
        bubbles: true,
        composed: true,
      }),
    );
  }

  private async setTimer(type: "after_video" | "minutes", minutes?: number): Promise<void> {
    this.busy = true;
    this.error = "";
    try {
      await api.post("/api/timer", { type, minutes: minutes ?? null });
      await this.refresh();
    } catch (err) {
      this.error = err instanceof ApiError ? String(err.body) : (err as Error).message;
    } finally {
      this.busy = false;
      this.dropdown?.hide?.();
    }
  }

  private async cancel(): Promise<void> {
    this.busy = true;
    try {
      await api.delete("/api/timer");
      this.timer = null;
      this.stopTick();
    } catch (err) {
      this.error = (err as Error).message;
    } finally {
      this.busy = false;
      this.dropdown?.hide?.();
    }
  }

  private formatRemaining(): string {
    if (!this.timer) return "";
    if (this.timer.timer_type === "after_video") return "After this video";
    if (this.timer.expires_at == null) return "";
    const remain = Math.max(0, this.timer.expires_at - this.now);
    const m = Math.floor(remain / 60);
    const s = remain % 60;
    return `${m}:${String(s).padStart(2, "0")}`;
  }

  override render() {
    const isActive = !!this.timer;
    const remaining = this.formatRemaining();
    return html`
      <wa-dropdown placement="bottom-end">
        <button
          slot="trigger"
          type="button"
          class="trigger ${isActive ? "active" : ""}"
          aria-label=${isActive ? `Sleep timer active: ${remaining}` : "Set sleep timer"}
        >
          ⏱
          ${isActive
            ? html`<span class="countdown" aria-live="polite">${remaining}</span>`
            : " Sleep timer"}
        </button>
        <div class="menu" role="menu">
          <button
            type="button"
            role="menuitem"
            ?disabled=${this.busy}
            @click=${() => void this.setTimer("after_video")}
          >
            Stop after this video
          </button>
          ${MINUTE_OPTIONS.map(
            (m) => html`
              <button
                type="button"
                role="menuitem"
                ?disabled=${this.busy}
                @click=${() => void this.setTimer("minutes", m)}
              >
                Stop in ${m} minutes
              </button>
            `,
          )}
          ${isActive
            ? html`<button
                type="button"
                role="menuitem"
                class="cancel"
                ?disabled=${this.busy}
                @click=${() => void this.cancel()}
              >
                Cancel timer
              </button>`
            : null}
        </div>
      </wa-dropdown>
      ${this.error ? html`<div class="error" role="alert">${this.error}</div>` : null}
    `;
  }
}

declare global {
  interface HTMLElementTagNameMap {
    "hometube-sleep-timer": SleepTimer;
  }
}
