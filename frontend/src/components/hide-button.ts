/**
 * <hometube-hide-button video-id="..." video-title="..." ...>
 *
 * Watch-page action, slotted into `<hometube-video-player slot="actions">`
 * so it sits alongside Like / Subscribe / Audio only in the player's
 * chrome row. On click POSTs `/api/hidden` with the current
 * video's metadata and redirects the browser to `/child/home` so the
 * back button doesn't return to the now-hidden video. The server
 * additionally denies further `/child/video/:id` navigation via the
 * `can_child_view` filter, so the redirect is the friendly path while
 * the hard guarantee is server-side.
 */

import { LitElement, html, css } from "lit";
import { customElement, property, state } from "lit/decorators.js";

import { api, ApiError } from "../services/api.js";

@customElement("hometube-hide-button")
export class HideButton extends LitElement {
  @property({ type: String, attribute: "video-id" })
  videoId = "";

  @property({ type: String, attribute: "video-title" })
  videoTitle = "";

  @property({ type: String, attribute: "channel-id" })
  channelId = "";

  @property({ type: String, attribute: "channel-title" })
  channelTitle = "";

  @property({ type: String, attribute: "thumbnail-url" })
  thumbnailUrl = "";

  @state() private busy = false;
  @state() private error = "";

  static styles = css`
    :host {
      display: inline-block;
    }
    button {
      display: inline-flex;
      align-items: center;
      gap: 0.4rem;
      padding: 0.4rem 0.75rem;
      border-radius: 0.375rem;
      border: 1px solid var(--wa-color-surface-border, #ccc);
      background: transparent;
      color: var(--wa-color-text-normal, inherit);
      font: inherit;
      cursor: pointer;
    }
    button:hover,
    button:focus-visible {
      background: var(--wa-color-surface-raised, #f3f4f6);
      outline: none;
    }
    button[disabled] {
      opacity: 0.6;
      cursor: not-allowed;
    }
    .error {
      color: var(--wa-color-danger-fill, #b91c1c);
      font-size: 0.85rem;
      margin-left: 0.5rem;
    }
  `;

  private async onClick() {
    if (!this.videoId || this.busy) return;
    this.busy = true;
    this.error = "";
    try {
      await api.post("/api/hidden", {
        video_id: this.videoId,
        video_title: this.videoTitle || null,
        channel_id: this.channelId || null,
        channel_title: this.channelTitle || null,
        video_thumbnail_url: this.thumbnailUrl || null,
      });
      // Leave a breadcrumb the undo toast on /child/home picks up.
      try {
        sessionStorage.setItem(
          "hometube:pendingHide",
          JSON.stringify({
            videoId: this.videoId,
            title: this.videoTitle || undefined,
            at: Date.now(),
          }),
        );
      } catch {
        /* ignore quota / privacy errors */
      }
      // Use replace() so the back button doesn't return to the now-hidden video.
      window.location.replace("/child/home");
    } catch (err) {
      this.busy = false;
      if (err instanceof ApiError) {
        this.error = `Couldn't hide (${err.status})`;
      } else {
        this.error = "Couldn't hide this video";
      }
    }
  }

  override render() {
    return html`
      <button type="button" ?disabled=${this.busy} @click=${this.onClick}>
        🙈 Hide this video
      </button>
      ${this.error ? html`<span class="error">${this.error}</span>` : null}
    `;
  }
}

declare global {
  interface HTMLElementTagNameMap {
    "hometube-hide-button": HideButton;
  }
}
