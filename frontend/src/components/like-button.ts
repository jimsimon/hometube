/**
 * <hometube-like-button video-id="...">
 *
 * Toggles a like on a video. POSTs `/api/likes/:videoId` to like and
 * DELETEs to unlike. Stays optimistic — the underlying YouTube push is
 * asynchronous.
 */

import { LitElement, html, css } from 'lit';
import { customElement, property, state } from 'lit/decorators.js';

import { ApiError, api } from '../services/api.js';
import type { LikeRow } from '../types/index.js';

@customElement('hometube-like-button')
export class LikeButton extends LitElement {
  @property({ type: String, attribute: 'video-id' })
  videoId = '';

  @state() private liked = false;
  @state() private busy = false;
  @state() private error = '';

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
    if (changed.has('videoId')) void this.refresh();
  }

  private async refresh(): Promise<void> {
    if (!this.videoId) return;
    try {
      const likes = await api.get<LikeRow[]>('/api/likes');
      this.liked = likes.some((l) => l.video_id === this.videoId);
    } catch {
      // Silent.
    }
  }

  private async onToggle(): Promise<void> {
    if (this.busy || !this.videoId) return;
    this.busy = true;
    this.error = '';
    const wasLiked = this.liked;
    try {
      if (wasLiked) {
        await api.delete(`/api/likes/${encodeURIComponent(this.videoId)}`);
        this.liked = false;
      } else {
        await api.post(`/api/likes/${encodeURIComponent(this.videoId)}`);
        this.liked = true;
      }
    } catch (err) {
      this.error =
        err instanceof ApiError ? String(err.body) : (err as Error).message;
    } finally {
      this.busy = false;
    }
  }

  override render() {
    return html`
      <button
        type="button"
        class=${this.liked ? 'liked' : ''}
        ?disabled=${this.busy}
        aria-pressed=${this.liked}
        aria-label=${this.liked ? 'Unlike video' : 'Like video'}
        @click=${this.onToggle}
      >
        <span class="icon" aria-hidden="true">${this.liked ? '♥' : '♡'}</span>
        ${this.liked ? 'Liked' : 'Like'}
      </button>
      ${this.error
        ? html`<span class="error" role="alert">${this.error}</span>`
        : null}
    `;
  }
}

declare global {
  interface HTMLElementTagNameMap {
    'hometube-like-button': LikeButton;
  }
}
