/**
 * <hometube-child-settings-form child-id="...">
 *
 * Edits the per-child player knobs: downloads on/off, max video
 * quality, playback-speed lock, autoplay on/off, and autoplay
 * consecutive-cap.
 */

import { LitElement, html, css, nothing } from "lit";
import { customElement, property, state } from "lit/decorators.js";

import { api, ApiError } from "../services/api.js";
import type { ChildSettings } from "../types/index.js";

import "./loading-spinner.js";
import "./error-banner.js";

const QUALITY_OPTIONS = ["unlimited", "480p", "720p", "1080p"] as const;

@customElement("hometube-child-settings-form")
export class ChildSettingsForm extends LitElement {
  @property({ type: Number, attribute: "child-id" })
  childId: number | null = null;

  @state() private settings: ChildSettings | null = null;
  @state() private saving = false;
  @state() private message = "";
  @state() private error = "";

  static styles = css`
    :host {
      display: block;
    }
    form {
      display: grid;
      gap: 0.75rem;
      max-width: 28rem;
      margin-block: 1rem;
    }
    label.row {
      display: flex;
      align-items: center;
      gap: 0.75rem;
      justify-content: space-between;
    }
    select,
    input[type="number"] {
      padding: 0.25rem 0.5rem;
      border: 1px solid var(--wa-color-surface-border, #ccc);
      border-radius: 0.375rem;
      background: var(--wa-color-surface-default);
      color: var(--wa-color-text-normal);
      font: inherit;
    }
    button {
      padding: 0.5rem 1rem;
      border-radius: 0.375rem;
      border: 1px solid var(--wa-color-surface-border, #ccc);
      background: var(--wa-color-brand-fill, #2563eb);
      color: white;
      font: inherit;
      cursor: pointer;
      justify-self: start;
    }
    .empty {
      color: var(--wa-color-text-quiet);
      font-style: italic;
    }
    .ok {
      color: var(--wa-color-success-fill, #15803d);
    }
    .error {
      color: var(--wa-color-danger-fill, #b91c1c);
    }
  `;

  override updated(changed: Map<string, unknown>): void {
    if (changed.has("childId") && this.childId != null) {
      void this.refresh();
    }
  }

  private async refresh(): Promise<void> {
    if (this.childId == null) return;
    try {
      this.settings = await api.get<ChildSettings>(`/api/children/${this.childId}/settings`);
    } catch (err) {
      this.error = (err as Error).message;
    }
  }

  private updateField<K extends keyof ChildSettings>(key: K, value: ChildSettings[K]): void {
    if (!this.settings) return;
    this.settings = { ...this.settings, [key]: value };
  }

  private async onSave(e: Event): Promise<void> {
    e.preventDefault();
    if (this.childId == null || !this.settings) return;
    this.saving = true;
    this.message = "";
    this.error = "";
    try {
      await api.put(`/api/children/${this.childId}/settings`, {
        downloads_enabled: this.settings.downloads_enabled,
        max_quality: this.settings.max_quality,
        playback_speed_locked: this.settings.playback_speed_locked,
        autoplay_enabled: this.settings.autoplay_enabled,
        autoplay_max_consecutive: this.settings.autoplay_max_consecutive,
        chromecast_enabled: this.settings.chromecast_enabled,
      });
      this.message = "Saved.";
    } catch (err) {
      this.error = err instanceof ApiError ? String(err.body) : (err as Error).message;
    } finally {
      this.saving = false;
    }
  }

  override render() {
    if (this.childId == null) {
      return html`<p class="empty">Pick a child to edit settings.</p>`;
    }
    if (!this.settings) {
      return html`<hometube-loading-spinner label="Loading settings…"></hometube-loading-spinner>`;
    }
    const s = this.settings;
    return html`
      <form @submit=${this.onSave}>
        <label class="row" for="downloads-enabled">
          Allow offline downloads
          <input
            id="downloads-enabled"
            type="checkbox"
            .checked=${s.downloads_enabled}
            @change=${(e: Event) =>
              this.updateField("downloads_enabled", (e.target as HTMLInputElement).checked)}
          />
        </label>

        <label class="row" for="max-quality">
          Maximum video quality
          <select
            id="max-quality"
            .value=${s.max_quality ?? "unlimited"}
            @change=${(e: Event) => {
              const v = (e.target as HTMLSelectElement).value;
              this.updateField(
                "max_quality",
                v === "unlimited" ? null : (v as "480p" | "720p" | "1080p"),
              );
            }}
          >
            ${QUALITY_OPTIONS.map(
              (q) =>
                html`<option value=${q} ?selected=${(s.max_quality ?? "unlimited") === q}>
                  ${q}
                </option>`,
            )}
          </select>
        </label>

        <label class="row" for="speed-locked">
          Lock playback speed
          <input
            id="speed-locked"
            type="checkbox"
            .checked=${s.playback_speed_locked}
            @change=${(e: Event) =>
              this.updateField("playback_speed_locked", (e.target as HTMLInputElement).checked)}
          />
        </label>

        <label class="row" for="chromecast-enabled">
          Allow Chromecast
          <input
            id="chromecast-enabled"
            type="checkbox"
            .checked=${s.chromecast_enabled}
            @change=${(e: Event) =>
              this.updateField("chromecast_enabled", (e.target as HTMLInputElement).checked)}
          />
        </label>

        <label class="row" for="autoplay-enabled">
          Autoplay next video
          <input
            id="autoplay-enabled"
            type="checkbox"
            .checked=${s.autoplay_enabled}
            @change=${(e: Event) =>
              this.updateField("autoplay_enabled", (e.target as HTMLInputElement).checked)}
          />
        </label>

        <label class="row" for="autoplay-max">
          Pause after this many videos in a row
          <input
            id="autoplay-max"
            type="number"
            min="1"
            max="20"
            step="1"
            .value=${s.autoplay_max_consecutive == null ? "" : String(s.autoplay_max_consecutive)}
            @input=${(e: Event) => {
              const v = (e.target as HTMLInputElement).value;
              this.updateField("autoplay_max_consecutive", v === "" ? null : Number(v));
            }}
          />
        </label>

        <button type="submit" ?disabled=${this.saving}>${this.saving ? "Saving…" : "Save"}</button>
        ${this.message ? html`<p class="ok" role="status">${this.message}</p>` : nothing}
        ${this.error
          ? html`<hometube-error-banner .message=${this.error}></hometube-error-banner>`
          : nothing}
      </form>
    `;
  }
}

declare global {
  interface HTMLElementTagNameMap {
    "hometube-child-settings-form": ChildSettingsForm;
  }
}
