/**
 * <hometube-like-button video-id="..." video-title="..." thumbnail-url="...">
 *
 * Toggles a like on a video. POSTs `/api/likes/:videoId` (packaging
 * the `video-title` and `thumbnail-url` attributes into the JSON body
 * so the backend doesn't need to call the discovery sidecar) to like,
 * and DELETEs to unlike.
 *
 * The attribute is named `video-title` rather than `title` because
 * `title` is the platform's tooltip attribute on every HTMLElement —
 * using it here would trigger the browser's tooltip instead of
 * populating the like row.
 */

import { LitElement, html, css } from "lit";
import { customElement, property, state } from "lit/decorators.js";

import { ApiError, api } from "../services/api.js";
import type { LikeRow } from "../types/index.js";

@customElement("hometube-like-button")
export class LikeButton extends LitElement {
  @property({ type: String, attribute: "video-id" })
  videoId = "";

  /**
   * Video title to persist on the like row. The player has this in
   * scope (`this.metadata.title`) at the moment the button is clicked,
   * so we pass it through instead of having the backend re-fetch it.
   * Optional — the backend tolerates a missing field.
   */
  @property({ type: String, attribute: "video-title" })
  videoTitle = "";

  /**
   * Thumbnail URL to persist on the like row. Same rationale as
   * `videoTitle`. Optional.
   */
  @property({ type: String, attribute: "thumbnail-url" })
  thumbnailUrl = "";

  /**
   * Channel id for the video. Persisted on the like row so the
   * server's `visible` flag can match against `allowlisted_channels`
   * (not just direct video-allowlist entries). Optional.
   */
  @property({ type: String, attribute: "channel-id" })
  channelId = "";

  /** Channel title for the video. Used to render the channel name on
   *  the liked-videos grid without a follow-up fetch. Optional. */
  @property({ type: String, attribute: "channel-title" })
  channelTitle = "";

  /**
   * Video length in seconds. Persisted on the like row so the liked
   * grid can render a duration badge. The player already has this via
   * `metadata.duration_seconds`; pass it through to avoid re-fetching
   * yt-dlp metadata. Optional; `0` is treated as "unknown".
   */
  @property({ type: Number, attribute: "duration-seconds" })
  durationSeconds = 0;

  @state() private liked = false;
  @state() private busy = false;
  @state() private error = "";

  /** Monotonic refresh token. Bumped on each `refresh()` call and on
   *  disconnect so a late `/api/likes` response from a previous video
   *  can't overwrite `this.liked` with a stale value. */
  private refreshToken = 0;

  static styles = css`
    :host {
      display: inline-block;
    }
    button {
      display: inline-flex;
      align-items: center;
      gap: 0.4rem;
      padding: 0.45rem 0.9rem;
      border-radius: 999px;
      border: 1px solid var(--wa-color-surface-border, #ccc);
      background: transparent;
      color: var(--wa-color-text-normal);
      font: inherit;
      cursor: pointer;
    }
    button.liked {
      background: var(--wa-color-brand-quiet, rgba(37, 99, 235, 0.15));
      color: var(--wa-color-brand-on-quiet);
    }
    .icon {
      font-size: 1.1em;
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

  override updated(changed: Map<string, unknown>): void {
    if (changed.has("videoId")) void this.refresh();
  }

  override disconnectedCallback(): void {
    this.refreshToken++;
    super.disconnectedCallback();
  }

  private async refresh(): Promise<void> {
    if (!this.videoId) return;
    const token = ++this.refreshToken;
    const videoId = this.videoId;
    try {
      const likes = await api.get<LikeRow[]>("/api/likes");
      // Discard if a newer refresh (or unmount) has happened, or if the
      // videoId we were querying for is no longer the current one.
      if (token !== this.refreshToken || videoId !== this.videoId) return;
      this.liked = likes.some((l) => l.video_id === videoId);
    } catch {
      // Silent.
    }
  }

  private async onToggle(): Promise<void> {
    if (this.busy || !this.videoId) return;
    this.busy = true;
    this.error = "";
    const wasLiked = this.liked;
    try {
      if (wasLiked) {
        await api.delete(`/api/likes/${encodeURIComponent(this.videoId)}`);
        this.liked = false;
      } else {
        // Pass metadata the player already has so the backend doesn't
        // need to call the discovery sidecar. Empty strings are sent as
        // null so the backend's `COALESCE` upsert preserves any
        // previously-stored values on re-like.
        const body = {
          title: this.videoTitle || null,
          thumbnail_url: this.thumbnailUrl || null,
          channel_id: this.channelId || null,
          channel_title: this.channelTitle || null,
          duration_seconds: this.durationSeconds > 0 ? this.durationSeconds : null,
        };
        await api.post(`/api/likes/${encodeURIComponent(this.videoId)}`, body);
        this.liked = true;
      }
    } catch (err) {
      this.error = err instanceof ApiError ? String(err.body) : (err as Error).message;
    } finally {
      this.busy = false;
    }
  }

  override render() {
    return html`
      <button
        type="button"
        class=${this.liked ? "liked" : ""}
        ?disabled=${this.busy}
        aria-pressed=${this.liked}
        aria-label=${this.liked ? "Unlike video" : "Like video"}
        @click=${this.onToggle}
      >
        <span class="icon" aria-hidden="true">${this.liked ? "♥" : "♡"}</span>
        ${this.liked ? "Liked" : "Like"}
      </button>
      ${this.error ? html`<span class="error" role="alert">${this.error}</span>` : null}
    `;
  }
}

declare global {
  interface HTMLElementTagNameMap {
    "hometube-like-button": LikeButton;
  }
}
