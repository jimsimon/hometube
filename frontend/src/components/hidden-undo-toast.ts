/**
 * <hometube-hidden-undo-toast>
 *
 * Global, self-mounting toast that listens (on `document`) for
 * `video-hidden` `CustomEvent`s dispatched by `<hometube-video-card>`
 * and the watch-page `<hometube-hide-button>`. Shows a 5-second undo
 * affordance that fires `DELETE /api/hidden/:videoId` on activation.
 *
 * The toast is deliberately decoupled from the cards: cards/buttons
 * own the hide action and disappear after dispatching their event;
 * this component owns the undo affordance and lives once per page
 * (inserted by `base-child.html`).
 */

import { LitElement, html, css } from "lit";
import { customElement, state } from "lit/decorators.js";

import { api, ApiError } from "../services/api.js";

const UNDO_TIMEOUT_MS = 5000;

@customElement("hometube-hidden-undo-toast")
export class HiddenUndoToast extends LitElement {
  @state() private videoId: string | null = null;
  @state() private hiddenTitle: string | null = null;
  @state() private busy = false;
  @state() private error = "";

  /**
   * `true` when the pending hide was performed on a different page
   * (watch-page redirect breadcrumb). Undo from that origin needs a
   * full reload because the in-page listings don't know about it.
   * In-page hides can be restored purely via the `video-unhidden`
   * event so we avoid the jarring reload.
   */
  private needsReload = false;

  private timer: number | null = null;

  static styles = css`
    :host {
      position: fixed;
      bottom: 1.25rem;
      left: 50%;
      transform: translateX(-50%);
      z-index: 9999;
      pointer-events: none;
    }
    .toast {
      display: inline-flex;
      align-items: center;
      gap: 0.75rem;
      padding: 0.6rem 0.9rem;
      border-radius: 0.5rem;
      background: rgba(17, 24, 39, 0.95);
      color: white;
      font-size: 0.9rem;
      box-shadow: 0 6px 24px rgba(0, 0, 0, 0.25);
      pointer-events: auto;
      max-width: min(90vw, 28rem);
    }
    button {
      background: transparent;
      color: inherit;
      border: 1px solid rgba(255, 255, 255, 0.4);
      border-radius: 0.375rem;
      padding: 0.25rem 0.6rem;
      font: inherit;
      cursor: pointer;
    }
    button:hover,
    button:focus-visible {
      background: rgba(255, 255, 255, 0.15);
      outline: none;
    }
    button[disabled] {
      opacity: 0.5;
      cursor: not-allowed;
    }
    .err {
      color: #fda4af;
      font-size: 0.8rem;
    }
    .msg {
      flex: 1;
      min-width: 0;
      white-space: nowrap;
      overflow: hidden;
      text-overflow: ellipsis;
    }
  `;

  override connectedCallback(): void {
    super.connectedCallback();
    document.addEventListener("video-hidden", this.onHidden as EventListener);
    // Watch-page hides redirect to /child/home before the toast can
    // render; they leave a `pendingHide` breadcrumb in sessionStorage
    // for us to consume on the next page.
    try {
      const raw = sessionStorage.getItem("hometube:pendingHide");
      if (raw) {
        sessionStorage.removeItem("hometube:pendingHide");
        const parsed = JSON.parse(raw) as { videoId: string; title?: string; at?: number };
        if (parsed.videoId && (!parsed.at || Date.now() - parsed.at < UNDO_TIMEOUT_MS)) {
          this.needsReload = true;
          this.showFor(parsed.videoId, parsed.title);
        }
      }
    } catch {
      /* ignore */
    }
  }

  override disconnectedCallback(): void {
    document.removeEventListener("video-hidden", this.onHidden as EventListener);
    this.clearTimer();
    super.disconnectedCallback();
  }

  private onHidden = (e: CustomEvent<{ videoId: string; title?: string }>) => {
    const videoId = e.detail?.videoId;
    if (!videoId) return;
    // Event-driven hide: in-page, so we don't need to reload to
    // restore the card — listings handle `video-unhidden` themselves.
    this.needsReload = false;
    this.showFor(videoId, e.detail?.title);
  };

  private showFor(videoId: string, title?: string) {
    this.videoId = videoId;
    this.hiddenTitle = title ?? null;
    this.busy = false;
    this.error = "";
    this.clearTimer();
    this.timer = window.setTimeout(() => this.dismiss(), UNDO_TIMEOUT_MS);
  }

  private clearTimer() {
    if (this.timer != null) {
      window.clearTimeout(this.timer);
      this.timer = null;
    }
  }

  private dismiss() {
    this.clearTimer();
    this.videoId = null;
    this.hiddenTitle = null;
    this.busy = false;
    this.needsReload = false;
    this.error = "";
  }

  private async onUndo() {
    if (!this.videoId || this.busy) return;
    this.busy = true;
    this.error = "";
    const id = this.videoId;
    const shouldReload = this.needsReload;
    try {
      await api.delete(`/api/hidden/${encodeURIComponent(id)}`);
      if (shouldReload) {
        // Hide came from a different page (e.g. watch-page redirect);
        // a full reload is the only way to pick up the un-hide on the
        // current view.
        window.location.reload();
        return;
      }
      // In-page hide: notify listings to restore the card, no reload.
      document.dispatchEvent(
        new CustomEvent("video-unhidden", {
          detail: { videoId: id },
          bubbles: true,
          composed: true,
        }),
      );
      this.dismiss();
    } catch (err) {
      if (err instanceof ApiError) {
        console.warn("Undo failed", err.status, err.body);
        this.error = `Couldn't undo (${err.status})`;
      } else {
        console.warn("Undo failed", err);
        this.error = "Couldn't undo";
      }
      this.busy = false;
    }
  }

  override render() {
    if (!this.videoId) return html``;
    const label = this.hiddenTitle ? `Hid "${this.hiddenTitle}".` : "Video hidden.";
    return html`
      <div class="toast" role="status" aria-live="polite">
        <span class="msg">${label}</span>
        ${this.error ? html`<span class="err" role="alert">${this.error}</span>` : null}
        <button type="button" ?disabled=${this.busy} @click=${this.onUndo}>Undo</button>
      </div>
    `;
  }
}

declare global {
  interface HTMLElementTagNameMap {
    "hometube-hidden-undo-toast": HiddenUndoToast;
  }
}
