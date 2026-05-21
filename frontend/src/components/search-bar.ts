/**
 * <hometube-search-bar>
 *
 * Compact search input + type filter intended for use inside the child
 * top navigation. While the user is typing, a debounced suggestions
 * request is sent to `/api/search?q=...&type=all&limit=5`; the top
 * results render in a dropdown and clicking one navigates straight to
 * the matched item. The same debounced tick also dispatches a
 * cancelable `search-change` CustomEvent so embedders (notably
 * `<hometube-search-results>`) can update in place without a reload.
 * Pressing Enter dispatches `search-change` first; if no listener
 * calls `preventDefault()` it falls back to navigating to
 * `/child/search?q=...&type=...` (the top-nav case).
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

import { LitElement, html, css, nothing } from "lit";
import { customElement, property, state } from "lit/decorators.js";

import { api, ApiError } from "../services/api.js";
import { debounce } from "../services/debounce.js";

// 300ms matches `<hometube-allowlist-manager>` and is the
// conventional search-debounce window. The previous 200ms was shorter
// than a typical keystroke interval (~50 WPM ≈ 220ms/char), so the
// timer fired between characters and the user effectively saw one
// /api/search request per keystroke — i.e. no coalescing at normal
// typing speeds.
const DEBOUNCE_MS = 300;
const MAX_SUGGESTIONS = 5;

type SearchKind = "all" | "channel" | "video";

interface ChannelHit {
  channel_id: string;
  channel_title: string;
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
    videos: VideoHit[];
  };
  next_page_token: string | null;
}

interface Suggestion {
  kind: "channel" | "video";
  href: string;
  title: string;
  subtitle: string | null;
}

@customElement("hometube-search-bar")
export class SearchBar extends LitElement {
  /** Initial query (used when rendered on the search page). */
  @property({ type: String, attribute: "initial-q" })
  initialQ = "";

  /** Initial kind filter. */
  @property({ type: String, attribute: "initial-type" })
  initialType: SearchKind = "all";

  @state() private query = "";
  @state() private kind: SearchKind = "all";
  @state() private suggestions: Suggestion[] = [];
  @state() private suggestionsOpen = false;
  @state() private highlighted = -1;
  @state() private compact = false;
  @state() private expanded = false;

  private lastFetchToken = 0;
  private mql: MediaQueryList | null = null;

  // The same debounced tick drives both the suggestion fetch and the
  // `search-change` event. They serve different consumers (the
  // dropdown vs. an embedded results view) and hit different
  // endpoints, so they're dispatched together on purpose — neither
  // should block the other. `fetchSuggestions()` is fire-and-forget
  // and `emitChange()` is synchronous, so ordering is irrelevant.
  private readonly scheduleFetch = debounce(() => {
    void this.fetchSuggestions();
    this.emitChange();
  }, DEBOUNCE_MS);

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
    input[type="search"] {
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
    .suggestion[data-highlighted="true"] {
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
    .search-icon-btn {
      display: inline-flex;
      align-items: center;
      justify-content: center;
      width: 2rem;
      height: 2rem;
      border-radius: 50%;
      border: 1px solid var(--wa-color-surface-border, #ccc);
      background: transparent;
      color: var(--wa-color-text-normal);
      cursor: pointer;
      font-size: 1rem;
    }
    .search-icon-btn:hover {
      background: var(--wa-color-surface-raised);
    }
    .expanded-overlay {
      position: fixed;
      inset: 0;
      z-index: 300;
      background: var(--wa-color-surface-default);
      padding: 1rem;
      display: flex;
      flex-direction: column;
      gap: 0.75rem;
    }
    .expanded-overlay .close-btn {
      align-self: flex-end;
      background: transparent;
      border: none;
      font-size: 1.5rem;
      cursor: pointer;
      color: var(--wa-color-text-normal);
      padding: 0.25rem;
    }
    @media (max-width: 48rem) {
      select {
        display: none;
      }
    }
  `;

  override connectedCallback(): void {
    super.connectedCallback();
    this.query = this.initialQ ?? "";
    this.kind = (this.initialType ?? "all") as SearchKind;
    document.addEventListener("click", this.onDocumentClick);
    this.mql = window.matchMedia("(max-width: 48rem)");
    this.compact = this.mql.matches;
    this.mql.addEventListener("change", this.onMediaChange);
  }

  override disconnectedCallback(): void {
    super.disconnectedCallback();
    document.removeEventListener("click", this.onDocumentClick);
    this.mql?.removeEventListener("change", this.onMediaChange);
    this.scheduleFetch.cancel();
  }

  private onMediaChange = (e: MediaQueryListEvent): void => {
    this.compact = e.matches;
    if (!e.matches) this.expanded = false;
  };

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
    if (!this.query.trim()) {
      this.scheduleFetch.cancel();
      this.suggestions = [];
      this.suggestionsOpen = false;
      this.emitChange();
      return;
    }
    this.scheduleFetch();
  };

  /**
   * Dispatch a `search-change` event so embedders (notably the
   * `<hometube-search-results>` page) can react to the debounced query
   * without requiring a form submit. The top-nav embedder simply
   * ignores it.
   */
  private emitChange(): boolean {
    // Scoped event: the only consumer listens directly on this
    // element, so there's no need to bubble or cross shadow boundaries.
    // `cancelable: true` lets an embedder (e.g. `<hometube-search-results>`)
    // claim the event with `preventDefault()` so that `onSubmit` can
    // skip its full-page navigation fallback. The return value is
    // `false` when the default was prevented.
    return this.dispatchEvent(
      new CustomEvent("search-change", {
        cancelable: true,
        detail: { q: this.query.trim(), kind: this.kind },
      }),
    );
  }

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
        kind: "channel",
        href: `/child/channel/${encodeURIComponent(ch.channel_id)}`,
        title: ch.channel_title,
        subtitle: "Channel",
      });
    }
    for (const v of res.results.videos.slice(0, MAX_SUGGESTIONS)) {
      out.push({
        kind: "video",
        href: `/child/video/${encodeURIComponent(v.video_id)}`,
        title: v.title,
        subtitle: v.channel_title ?? "Video",
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
    // Fire the in-page event first; if an embedder handles it (calls
    // `preventDefault()`) we skip the full-page navigation so Enter
    // behaves consistently with the debounced auto-search.
    this.scheduleFetch.cancel();
    const notHandled = this.emitChange();
    if (!notHandled) return;
    const url = `/child/search?q=${encodeURIComponent(trimmed)}&type=${encodeURIComponent(
      this.kind,
    )}`;
    window.location.href = url;
  };

  private onKeydown = (event: KeyboardEvent): void => {
    if (!this.suggestionsOpen) return;
    if (event.key === "ArrowDown") {
      event.preventDefault();
      this.highlighted = Math.min(this.suggestions.length - 1, this.highlighted + 1);
    } else if (event.key === "ArrowUp") {
      event.preventDefault();
      this.highlighted = Math.max(-1, this.highlighted - 1);
    } else if (event.key === "Escape") {
      this.suggestionsOpen = false;
      this.highlighted = -1;
    }
  };

  private onSuggestionClick = (s: Suggestion): void => {
    window.location.href = s.href;
  };

  private openExpanded = (): void => {
    this.expanded = true;
    // Focus the input after render
    requestAnimationFrame(() => {
      const input = this.renderRoot.querySelector<HTMLInputElement>("#search-input");
      input?.focus();
    });
  };

  private closeExpanded = (): void => {
    this.expanded = false;
  };

  override render() {
    // Compact mode: show only a search icon button
    if (this.compact && !this.expanded) {
      return html`
        <button
          type="button"
          class="search-icon-btn"
          aria-label="Open search"
          @click=${this.openExpanded}
        >
          🔍
        </button>
      `;
    }

    // Expanded overlay on mobile or always-visible on desktop
    const form = html`
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
            aria-expanded=${this.suggestionsOpen ? "true" : "false"}
            aria-controls="search-suggestions"
            aria-activedescendant=${this.highlighted >= 0 ? `suggestion-${this.highlighted}` : ""}
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
                    aria-selected=${idx === this.highlighted ? "true" : "false"}
                    data-highlighted=${idx === this.highlighted ? "true" : "false"}
                    class="suggestion"
                    @mousedown=${(e: Event) => {
                      e.preventDefault();
                      this.onSuggestionClick(s);
                    }}
                  >
                    <span>${s.title}</span>
                    ${s.subtitle ? html`<span class="meta">${s.subtitle}</span>` : nothing}
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
            // Re-emit immediately so the embedded results view
            // re-fetches with the new filter without a debounce wait.
            this.scheduleFetch.cancel();
            this.emitChange();
          }}
        >
          <option value="all">All</option>
          <option value="channel">Channels</option>
          <option value="video">Videos</option>
        </select>
      </form>
    `;

    // On mobile expanded mode, wrap the form in a fullscreen overlay
    if (this.compact && this.expanded) {
      return html`
        <div
          class="expanded-overlay"
          @keydown=${(e: KeyboardEvent) => {
            if (e.key === "Escape") this.closeExpanded();
          }}
        >
          <button
            type="button"
            class="close-btn"
            aria-label="Close search"
            @click=${this.closeExpanded}
          >
            ✕
          </button>
          ${form}
        </div>
      `;
    }

    return form;
  }
}

declare global {
  interface HTMLElementTagNameMap {
    "hometube-search-bar": SearchBar;
  }
}
