/**
 * <hometube-blocklist-manager child-id="...">
 *
 * Lets the parent paste a YouTube video URL or ID, optionally with a
 * reason, to add it to the child's blocklist. Blocking always wins over
 * any allowlist entry.
 */

import { LitElement, html, css, nothing } from 'lit';
import { customElement, property, state } from 'lit/decorators.js';

import { ApiError, api } from '../services/api.js';
import type { BlockedVideo } from '../types/index.js';

@customElement('hometube-blocklist-manager')
export class BlocklistManager extends LitElement {
  @property({ type: Number, attribute: 'child-id' })
  childId: number | null = null;

  @state() private videos: BlockedVideo[] = [];
  @state() private input = '';
  @state() private reason = '';
  @state() private busy = false;
  @state() private error = '';

  static styles = css`
    :host {
      display: block;
    }
    form {
      display: grid;
      gap: 0.5rem;
      grid-template-columns: 1fr 1fr auto;
      align-items: end;
      margin-block: 1rem;
    }
    label {
      display: grid;
      gap: 0.25rem;
      font-size: 0.9rem;
    }
    input[type='text'] {
      padding: 0.5rem;
      border: 1px solid var(--wa-color-surface-border, #ccc);
      border-radius: 0.375rem;
      background: var(--wa-color-surface-default);
      color: var(--wa-color-text-normal);
      font: inherit;
    }
    button {
      padding: 0.5rem 0.75rem;
      border-radius: 0.375rem;
      border: 1px solid var(--wa-color-surface-border, #ccc);
      background: var(--wa-color-brand-fill, #2563eb);
      color: white;
      font: inherit;
      cursor: pointer;
    }
    button.secondary {
      background: transparent;
      color: var(--wa-color-text-normal);
    }
    ul {
      list-style: none;
      padding: 0;
      margin: 0;
      display: grid;
      gap: 0.5rem;
    }
    li {
      display: flex;
      align-items: center;
      justify-content: space-between;
      gap: 1rem;
      padding: 0.5rem 0.75rem;
      border: 1px solid var(--wa-color-surface-border);
      border-radius: 0.375rem;
    }
    .empty {
      color: var(--wa-color-text-quiet);
      font-style: italic;
    }
    .error {
      color: var(--wa-color-danger-fill, #b91c1c);
    }
  `;

  override updated(changed: Map<string, unknown>): void {
    if (changed.has('childId') && this.childId != null) {
      void this.refresh();
    }
  }

  private async refresh(): Promise<void> {
    if (this.childId == null) return;
    try {
      this.videos = await api.get<BlockedVideo[]>(
        `/api/children/${this.childId}/blocked`,
      );
    } catch (err) {
      this.error = (err as Error).message;
    }
  }

  private parseId(input: string): string {
    // Same heuristics as the server's `parse_video_id`.
    const trimmed = input.trim();
    const match = trimmed.match(
      /(?:youtu\.be\/|youtube\.com\/(?:watch\?(?:.*&)?v=|embed\/|shorts\/))([\w-]{6,})/,
    );
    if (match) return match[1] ?? trimmed;
    return trimmed;
  }

  private async onAdd(e: Event): Promise<void> {
    e.preventDefault();
    if (this.childId == null || !this.input.trim()) return;
    this.busy = true;
    this.error = '';
    try {
      await api.post(`/api/children/${this.childId}/blocked`, {
        video_id: this.parseId(this.input),
        reason: this.reason.trim() || null,
      });
      this.input = '';
      this.reason = '';
      await this.refresh();
    } catch (err) {
      this.error = err instanceof ApiError ? String(err.body) : (err as Error).message;
    } finally {
      this.busy = false;
    }
  }

  private async onRemove(videoId: string): Promise<void> {
    if (this.childId == null) return;
    try {
      await api.delete(
        `/api/children/${this.childId}/blocked/${encodeURIComponent(videoId)}`,
      );
      await this.refresh();
    } catch (err) {
      this.error = (err as Error).message;
    }
  }

  override render() {
    if (this.childId == null) {
      return html`<p class="empty">Pick a child to manage their blocklist.</p>`;
    }
    return html`
      <form @submit=${this.onAdd}>
        <label for="block-id">
          Video URL or ID
          <input
            id="block-id"
            type="text"
            placeholder="https://www.youtube.com/watch?v=..."
            .value=${this.input}
            @input=${(e: Event) =>
              (this.input = (e.target as HTMLInputElement).value)}
            required
          />
        </label>
        <label for="block-reason">
          Reason (optional)
          <input
            id="block-reason"
            type="text"
            .value=${this.reason}
            @input=${(e: Event) =>
              (this.reason = (e.target as HTMLInputElement).value)}
          />
        </label>
        <button type="submit" ?disabled=${this.busy}>
          ${this.busy ? 'Blocking…' : 'Block video'}
        </button>
      </form>

      ${this.error ? html`<p class="error" role="alert">${this.error}</p>` : nothing}

      ${this.videos.length === 0
        ? html`<p class="empty">No blocked videos.</p>`
        : html`
            <ul>
              ${this.videos.map(
                (v) => html`
                  <li>
                    <div>
                      <strong>${v.video_title ?? v.video_id}</strong>
                      ${v.reason
                        ? html`<div class="empty">— ${v.reason}</div>`
                        : nothing}
                    </div>
                    <button
                      type="button"
                      class="secondary"
                      @click=${() => void this.onRemove(v.video_id)}
                    >
                      Unblock
                    </button>
                  </li>
                `,
              )}
            </ul>
          `}
    `;
  }
}

declare global {
  interface HTMLElementTagNameMap {
    'hometube-blocklist-manager': BlocklistManager;
  }
}
