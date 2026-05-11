/**
 * <hometube-search-bar>
 *
 * Compact search input + type filter intended for use inside the child
 * top navigation. Pressing Enter (or clicking the submit button)
 * navigates to `/child/search?q=...&type=...`. While the user is
 * typing, a debounced suggestions request is sent to
 * `/api/search?q=...&type=all&limit=5`; the top results render in a
 * dropdown and clicking one navigates straight to the matched item.
 *
 * The component is intentionally framework-agnostic — it doesn't
 * depend on a specific dropdown library so it can be embedded anywhere.
 *
 * Accessibility:
 *   - The search box has an associated `<label>` (visually hidden).
 *   - Suggestions are exposed as a `role="listbox"` with each entry
 *     `role="option"` so screen readers announce navigation.
 *   - The current selection is reflected via `aria-activedescendant`.
 */

import { LitElement, html, css, nothing } from 'lit';
import { customElement, property, state, query } from 'lit/decorators.js';

import { api, ApiError } from '../services/api.js';

const DEBOUNCE_MS = 200;
const MAX_SUGGESTIONS = 5;

type SearchKind = 'all' | 'channel' | 'playlist' | 'video';

interface ChannelHit {
  channel_id: string;
  channel_title: string;
}

interface PlaylistHit {
  playlist_id: string;
  playlist_title: string;
  source: 'allowlist' | 'own' | 'family';
}

interface VideoHit {
  video_id: string;
  title: string;
  channel_title: string | null;
}

interface SearchApiResponse {
  q: string;
  kind: string;
  results: {
    channels: ChannelHit[];
    playlists: PlaylistHit[];
    videos: VideoHit[];
  };
  next_page_token: string | null;
}

interface Suggestion {
  kind: 'channel' | 'playlist' | 'video';
  href: string;
  title: string;
  subtitle: string | null;
}

@customElement('hometube-search-bar')
export class SearchBar extends LitElement {
  /** Initial query (used when rendered on the search page). */
  @property({ type: String, attribute: 'initial-q' })
  initialQ = '';

  /** Initial kind filter. */
  @property({ type: String, attribute: 'initial-type' })
  initialType: SearchKind = 'all';

  @state() private query = '';
  @state() private kind: SearchKind = 'all';
  @state() private suggestions: Suggestion[] = [];
  @state() private suggestionsOpen = false;
  @state() private highlighted = -1;

  @query('input[type="search"]') private input!: HTMLInputElement;

  private debounceTimer: number | null = null;
  private lastFetchToken = 0;

  static styles = css`
    :host {
      display: block;
    }
    form {
      position: relative;
      display: flex;
      gap: 0.5rem;
      align-items: center;
    }
    .input-wrapper {
      flex: 1;
      position: relative;
    }
    label {
      position: absolute;
      width: 1px;
      height: 1px;
      overflow: hidden;
      clip: rect(0 0 0 0);
      white-space: nowrap;
    }
    input[type='search'] {
      width: 100%;
      padding: 0.5rem 0.75rem;
      border-radius: 0.375rem;
      border: 1px solid var(--wa-color-surface-border, #ccc);
      background: var(--wa-color-surface-default);
      color: var(--wa-color-text-normal);
      font: inherit;
    }
    select {
      padding: 0.5rem;
      border-radius: 0.375rem;
      border: 1px solid var(--wa-color-surface-border, #ccc);
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
    .suggestions {
      position: absolute;
      top: calc(100% + 0.25rem);
      left: 0;
      right: 0;
      max-height: 18rem;
      overflow-y: auto;
      background: var(--wa-color-surface-default);
      border: 1px solid var(--wa-color-surface-border, #ccc);
      border-radius: 0.375rem;
      box-shadow: 0 0.5rem 1.5rem rgba(0, 0, 0, 0.15);
      list-style: none;
      padding: 0.25rem 0;
      margin: 0;
      z-index: 50;
    }
    .suggestion {
      display: flex;
      flex-direction: column;
      gap: 0.125rem;
      padding: 0.5rem 0.75rem;
      cursor: pointer;
      text-decoration: none;
      color: inherit;
    }
    .suggestion:hover,
    .suggestion[data-highlighted='true'] {
      background: var(--wa-color-surface-raised);
    }
    .suggestion .meta {
      font-size: 0.75rem;
      color: var(--wa-color-text-quiet);
    }
    .empty {
      padding: 0.5rem 0.75rem;
      color: var(--wa-color-text-quiet);
      font-size: 0.85rem;
    }
  `;

  override connectedCallback(): void {
    super.connectedCallback();
    this.query = this.initialQ ?? '';
    this.kind = (this.initialType ?? 'all') as SearchKind;
    document.addEventListener('click', this.onDocumentClick);
  }

  override disconnectedCallback(): void {
    super.disconnectedCallback();
    document.removeEventListener('click', this.onDocumentClick);
    if (this.debounceTimer != null) {
      window.clearTimeout(this.debounceTimer);
    }
  }

  private onDocumentClick = (event: MouseEvent): void => {
    if (!this.suggestionsOpen) return;
    const path = event.composedPath();
    if (!path.includes(this)) {
      this.suggestionsOpen = false;
    }
  };

  private onInput = (event: Event): void => {
    this.query = (event.target as HTMLInputElement).value;
    this.highlighted = -1;
    if (this.debounceTimer != null) {
      window.clearTimeout(this.debounceTimer);
    }
    if (!this.query.trim()) {
      this.suggestions = [];
      this.suggestionsOpen = false;
      return;
    }
    this.debounceTimer = window.setTimeout(() => {
      void this.fetchSuggestions();
    }, DEBOUNCE_MS);
  };

  private async fetchSuggestions(): Promise<void> {
    const token = ++this.lastFetchToken;
    const q = this.query.trim();
    if (!q) return;
    try {
      const url = `/api/search?q=${encodeURIComponent(q)}&type=all&limit=${MAX_SUGGESTIONS}`;
      const res = await api.get<SearchApiResponse>(url);
      if (token !== this.lastFetchToken) return; // a newer request superseded us
      this.suggestions = this.flattenSuggestions(res);
      this.suggestionsOpen = this.suggestions.length > 0;
    } catch (err) {
      if (err instanceof ApiError && err.status === 401) {
        // not signed in — ignore
        return;
      }
      // Suggestions are best-effort; never surface errors here.
      this.suggestions = [];
      this.suggestionsOpen = false;
    }
  }

  private flattenSuggestions(res: SearchApiResponse): Suggestion[] {
    const out: Suggestion[] = [];
    for (const ch of res.results.channels.slice(0, 2)) {
      out.push({
        kind: 'channel',
        href: `/child/channel/${encodeURIComponent(ch.channel_id)}`,
        title: ch.channel_title,
        subtitle: 'Channel',
      });
    }
    for (const pl of res.results.playlists.slice(0, 2)) {
      const id = pl.playlist_id;
      const href = id.startsWith('family:')
        ? `/child/playlists`
        : `/child/playlist/${encodeURIComponent(id)}`;
      out.push({
        kind: 'playlist',
        href,
        title: pl.playlist_title,
        subtitle: 'Playlist',
      });
    }
    for (const v of res.results.videos.slice(0, MAX_SUGGESTIONS)) {
      out.push({
        kind: 'video',
        href: `/child/video/${encodeURIComponent(v.video_id)}`,
        title: v.title,
        subtitle: v.channel_title ?? 'Video',
      });
    }
    return out.slice(0, MAX_SUGGESTIONS);
  }

  private onSubmit = (event: Event): void => {
    event.preventDefault();
    const trimmed = this.query.trim();
    if (!trimmed) return;
    this.suggestionsOpen = false;
    if (this.highlighted >= 0 && this.suggestions[this.highlighted]) {
      window.location.href = this.suggestions[this.highlighted].href;
      return;
    }
    const url = `/child/search?q=${encodeURIComponent(trimmed)}&type=${encodeURIComponent(
      this.kind,
    )}`;
    window.location.href = url;
  };

  private onKeydown = (event: KeyboardEvent): void => {
    if (!this.suggestionsOpen) return;
    if (event.key === 'ArrowDown') {
      event.preventDefault();
      this.highlighted = Math.min(
        this.suggestions.length - 1,
        this.highlighted + 1,
      );
    } else if (event.key === 'ArrowUp') {
      event.preventDefault();
      this.highlighted = Math.max(-1, this.highlighted - 1);
    } else if (event.key === 'Escape') {
      this.suggestionsOpen = false;
      this.highlighted = -1;
    }
  };

  private onSuggestionClick = (s: Suggestion): void => {
    window.location.href = s.href;
  };

  override render() {
    return html`
      <form role="search" @submit=${this.onSubmit} @keydown=${this.onKeydown}>
        <div class="input-wrapper">
          <label for="search-input">Search</label>
          <input
            id="search-input"
            type="search"
            placeholder="Search videos…"
            .value=${this.query}
            autocomplete="off"
            aria-autocomplete="list"
            aria-expanded=${this.suggestionsOpen ? 'true' : 'false'}
            aria-controls="search-suggestions"
            aria-activedescendant=${this.highlighted >= 0
              ? `suggestion-${this.highlighted}`
              : ''}
            @input=${this.onInput}
            @focus=${() => {
              if (this.suggestions.length > 0) this.suggestionsOpen = true;
            }}
          />
          ${this.suggestionsOpen
            ? html`<ul
                id="search-suggestions"
                class="suggestions"
                role="listbox"
                aria-label="Search suggestions"
              >
                ${this.suggestions.map(
                  (s, idx) => html`<li
                    id="suggestion-${idx}"
                    role="option"
                    aria-selected=${idx === this.highlighted ? 'true' : 'false'}
                    data-highlighted=${idx === this.highlighted ? 'true' : 'false'}
                    class="suggestion"
                    @mousedown=${(e: Event) => {
                      e.preventDefault();
                      this.onSuggestionClick(s);
                    }}
                  >
                    <span>${s.title}</span>
                    ${s.subtitle
                      ? html`<span class="meta">${s.subtitle}</span>`
                      : nothing}
                  </li>`,
                )}
              </ul>`
            : nothing}
        </div>
        <label class="sr-only" for="search-type">Search in</label>
        <select
          id="search-type"
          .value=${this.kind}
          aria-label="Search filter"
          @change=${(e: Event) => {
            this.kind = (e.target as HTMLSelectElement).value as SearchKind;
          }}
        >
          <option value="all">All</option>
          <option value="channel">Channels</option>
          <option value="playlist">Playlists</option>
          <option value="video">Videos</option>
        </select>
        <button type="submit">Search</button>
      </form>
    `;
  }
}

declare global {
  interface HTMLElementTagNameMap {
    'hometube-search-bar': SearchBar;
  }
}
