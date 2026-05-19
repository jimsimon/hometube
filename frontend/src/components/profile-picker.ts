/**
 * <hometube-profile-picker>
 *
 * Netflix-style avatar grid for the `/profiles` page.
 *
 *   - Fetches `/api/auth/profiles`
 *   - Renders one tile per account with avatar (or initials fallback),
 *     display name, and a role badge
 *   - Tapping any profile (parent or child) opens
 *     `<hometube-pin-entry-dialog>`. Switching to a child requires any
 *     parent's PIN; switching to a parent requires that parent's own PIN.
 *   - Parents without a configured PIN show a "Set PIN required" badge
 *     and clicking them links straight to `/setup/pin?for_new_parent=1`
 *     (the tile is not selectable for switching)
 */

import { LitElement, html, css, nothing } from "lit";
import { customElement, state } from "lit/decorators.js";

import { ApiError, api } from "../services/api.js";

import "./pin-entry-dialog.js";
import "./loading-spinner.js";
import "./error-banner.js";

interface ProfileSummary {
  id: number;
  display_name: string;
  avatar_url: string | null;
  account_type: "parent" | "child";
  has_pin: boolean;
}

const TILE_BG_COLORS = ["#2563eb", "#7c3aed", "#dc2626", "#16a34a", "#ca8a04", "#0891b2"];

@customElement("hometube-profile-picker")
export class ProfilePicker extends LitElement {
  @state() private profiles: ProfileSummary[] = [];
  @state() private loading = true;
  @state() private error = "";
  @state() private pinTarget: ProfileSummary | null = null;

  static styles = css`
    :host {
      display: block;
    }
    .grid {
      display: grid;
      gap: 1.5rem;
      grid-template-columns: repeat(auto-fit, minmax(8rem, 1fr));
      margin-top: 2rem;
    }
    .tile {
      display: flex;
      flex-direction: column;
      align-items: center;
      gap: 0.5rem;
      padding: 1rem 0.5rem;
      border: 0;
      background: transparent;
      color: inherit;
      cursor: pointer;
      font: inherit;
      border-radius: 0.5rem;
      transition: transform 0.15s ease;
    }
    .tile:hover,
    .tile:focus-visible {
      transform: scale(1.05);
      outline: 2px solid var(--wa-color-brand-fill, #2563eb);
      outline-offset: 4px;
    }
    .tile[disabled] {
      cursor: not-allowed;
      opacity: 0.7;
    }
    .avatar {
      width: 6rem;
      height: 6rem;
      border-radius: 50%;
      background: var(--wa-color-surface-raised);
      color: white;
      display: grid;
      place-items: center;
      font-size: 2rem;
      font-weight: 700;
      overflow: hidden;
    }
    .avatar img {
      width: 100%;
      height: 100%;
      object-fit: cover;
    }
    .name {
      font-size: 1.05rem;
      font-weight: 600;
    }
    .badges {
      display: flex;
      gap: 0.25rem;
      flex-wrap: wrap;
      justify-content: center;
    }
    .badge {
      font-size: 0.7rem;
      padding: 0.05rem 0.4rem;
      border-radius: 999px;
      background: var(--wa-color-surface-raised);
      color: var(--wa-color-text-quiet);
      text-transform: uppercase;
    }
    .badge.parent {
      background: var(--wa-color-brand-quiet, rgba(37, 99, 235, 0.18));
      color: var(--wa-color-brand-on-quiet, #1d4ed8);
    }
    .badge.warn {
      background: var(--wa-color-warning-quiet, rgba(245, 158, 11, 0.18));
      color: var(--wa-color-warning-on-quiet, #92400e);
    }
    .empty,
    .error {
      text-align: center;
      margin-top: 2rem;
    }
    .error {
      color: var(--wa-color-danger-fill, #b91c1c);
    }
  `;

  override connectedCallback(): void {
    super.connectedCallback();
    void this.load();
  }

  private async load(): Promise<void> {
    this.loading = true;
    this.error = "";
    try {
      this.profiles = await api.get<ProfileSummary[]>("/api/auth/profiles");
    } catch (err) {
      if (err instanceof ApiError && typeof err.body === "string") {
        this.error = err.body;
      } else if (err instanceof Error) {
        this.error = err.message;
      } else {
        this.error = "Could not load profiles";
      }
    } finally {
      this.loading = false;
    }
  }

  private initials(name: string): string {
    const parts = name.trim().split(/\s+/);
    return parts
      .slice(0, 2)
      .map((p) => p[0]?.toUpperCase() ?? "")
      .join("");
  }

  private avatarBg(id: number): string {
    return TILE_BG_COLORS[id % TILE_BG_COLORS.length] ?? "#2563eb";
  }

  private async onTileClick(profile: ProfileSummary): Promise<void> {
    if (profile.account_type === "parent") {
      if (!profile.has_pin) {
        // Can't switch into a PIN-less parent. The tile is rendered
        // with `disabled`, but click handlers still fire on touch
        // devices — show a friendly explanation instead of failing
        // silently. The plan calls for "Set PIN required" with a link
        // to the set-pin page; the parent must sign in via Google for
        // that, so we surface a hint and let them use the dashboard.
        this.error =
          `${profile.display_name} has no PIN yet. ` +
          `Sign in as them once and set one from the parent dashboard.`;
        return;
      }
      this.pinTarget = profile;
      return;
    }
    // Children also require a parent's PIN before switching.
    this.pinTarget = profile;
  }

  override render() {
    if (this.loading)
      return html`<hometube-loading-spinner label="Loading profiles…"></hometube-loading-spinner>`;
    if (this.error)
      return html`<hometube-error-banner .message=${this.error}></hometube-error-banner>`;
    if (this.profiles.length === 0)
      return html`<p class="empty">No profiles yet — finish setup first.</p>`;

    return html`
      <div class="grid" role="list">${this.profiles.map((p) => this.renderTile(p))}</div>

      <hometube-pin-entry-dialog
        ?open=${this.pinTarget != null}
        account-id=${this.pinTarget?.id ?? 0}
        display-name=${this.pinTarget?.display_name ?? ""}
        ?require-parent-pin=${this.pinTarget?.account_type === "child"}
        @pin-cancelled=${() => (this.pinTarget = null)}
      ></hometube-pin-entry-dialog>
    `;
  }

  private renderTile(p: ProfileSummary) {
    const needsPin = p.account_type === "parent" && !p.has_pin;
    const label = needsPin
      ? `${p.display_name} (set PIN required)`
      : p.account_type === "parent"
        ? `Switch to ${p.display_name} (parent — PIN required)`
        : `Switch to ${p.display_name} (parent PIN required)`;
    return html`
      <button
        type="button"
        role="listitem"
        class="tile"
        aria-label=${label}
        ?disabled=${needsPin}
        @click=${() => this.onTileClick(p)}
      >
        <div
          class="avatar"
          aria-hidden="true"
          style=${p.avatar_url ? "" : `background: ${this.avatarBg(p.id)}`}
        >
          ${p.avatar_url
            ? html`<img src=${p.avatar_url} alt="" />`
            : html`${this.initials(p.display_name)}`}
        </div>
        <div class="name">${p.display_name}</div>
        <div class="badges">
          <span class=${`badge ${p.account_type}`}>${p.account_type}</span>
          ${needsPin ? html`<span class="badge warn">Set PIN required</span>` : nothing}
        </div>
      </button>
    `;
  }
}

declare global {
  interface HTMLElementTagNameMap {
    "hometube-profile-picker": ProfilePicker;
  }
}
