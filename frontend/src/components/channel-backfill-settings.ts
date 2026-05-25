/**
 * <hometube-channel-backfill-settings>
 *
 * Parent-only control panel for the background channel-archive backfill.
 *
 * The backfill loop runs `yt-dlp --flat-playlist` against allowlisted
 * channels (default cadence: 1 channel/hour, 30-day re-backfill) and
 * writes results into the unified `channel_videos` table. Mirrors the
 * shape of `<hometube-feed-refresher-settings>` so the parent system
 * page hosts the two sibling panels with a consistent look + feel.
 *
 * Also surfaces a per-channel state table (read from the consolidated
 * `/api/admin/channel-sync-state` endpoint, filtered to backfill
 * columns) with per-row "Run now" and "Unshelve" buttons.
 */

import { LitElement, css, html, nothing } from "lit";
import { customElement, state } from "lit/decorators.js";

import { api } from "../services/api.js";

interface BackfillSettings {
  enabled: boolean;
  min_gap_between_channels_s: number;
  re_backfill_interval_s: number;
  subprocess_timeout_s: number;
  ytdlp_sleep_requests_s: number;
  ytdlp_sleep_interval_s: number;
  ytdlp_max_sleep_interval_s: number;
  max_consecutive_errors_before_shelve: number;
  notify_on_shelve: boolean;
  idle_tick_s: number;
  raw?: {
    enabled: string | null;
    min_gap_between_channels_s: string | null;
    re_backfill_interval_s: string | null;
    subprocess_timeout_s: string | null;
    ytdlp_sleep_requests_s: string | null;
    ytdlp_sleep_interval_s: string | null;
    ytdlp_max_sleep_interval_s: string | null;
    max_consecutive_errors_before_shelve: string | null;
    notify_on_shelve: string | null;
    idle_tick_s: string | null;
  };
}

/** Numeric tunables that have a [min, max] range. The two booleans
 *  (`enabled`, `notify_on_shelve`) are handled separately. */
type NumericKey =
  | "min_gap_between_channels_s"
  | "re_backfill_interval_s"
  | "subprocess_timeout_s"
  | "ytdlp_sleep_requests_s"
  | "ytdlp_sleep_interval_s"
  | "ytdlp_max_sleep_interval_s"
  | "max_consecutive_errors_before_shelve"
  | "idle_tick_s";

// Per-field range limits. Keep in sync with `RANGE_*` consts in
// src/services/channel_backfill.rs and the validator in
// src/routes/channel_backfill.rs::admin_put_settings.
const RANGES: Record<NumericKey, [number, number]> = {
  min_gap_between_channels_s: [300, 86_400],
  re_backfill_interval_s: [86_400, 31_536_000],
  subprocess_timeout_s: [60, 14_400],
  ytdlp_sleep_requests_s: [0, 10],
  ytdlp_sleep_interval_s: [0, 10],
  ytdlp_max_sleep_interval_s: [0, 30],
  max_consecutive_errors_before_shelve: [1, 20],
  idle_tick_s: [5, 3600],
};

/** Subset of the `/api/admin/channel-sync-state` row used by the
 *  per-channel table. We pick out the backfill-relevant columns
 *  (the parallel freshness-tier columns are surfaced by the
 *  feed-refresher panel). */
interface ChannelBackfillRow {
  channel_id: string;
  channel_title: string | null;
  item_count: number;
  archived_count: number;
  backfill_status: string;
  backfill_last_completed_at: number | null;
  backfill_last_error: string | null;
  backfill_consecutive_errors: number;
  backfill_next_at: number;
}

@customElement("hometube-channel-backfill-settings")
export class ChannelBackfillSettings extends LitElement {
  @state() private settings: BackfillSettings | null = null;
  @state() private rows: ChannelBackfillRow[] = [];
  @state() private busy = false;
  @state() private status = "";

  /** Editable string copies of numeric inputs (mirrors the
   *  feed-refresher panel pattern — keeping the string form lets us
   *  treat an empty input as "leave unchanged"). */
  @state() private form: Record<NumericKey, string> = {
    min_gap_between_channels_s: "",
    re_backfill_interval_s: "",
    subprocess_timeout_s: "",
    ytdlp_sleep_requests_s: "",
    ytdlp_sleep_interval_s: "",
    ytdlp_max_sleep_interval_s: "",
    max_consecutive_errors_before_shelve: "",
    idle_tick_s: "",
  };

  @state() private enabled = true;
  @state() private notifyOnShelve = true;

  static styles = css`
    :host {
      display: block;
    }
    .card {
      padding: 1rem;
      border: 1px solid var(--wa-color-surface-border, #ddd);
      border-radius: 0.5rem;
      background: var(--wa-color-surface-default);
    }
    h3 {
      margin-top: 0;
    }
    .grid {
      display: grid;
      grid-template-columns: max-content 1fr;
      column-gap: 1rem;
      row-gap: 0.5rem;
      align-items: center;
      max-width: 50rem;
    }
    label {
      font-weight: 500;
    }
    .help {
      grid-column: 2;
      font-size: 0.85rem;
      color: var(--wa-color-text-quiet);
      margin: 0;
    }
    input[type="number"],
    input[type="checkbox"] {
      font: inherit;
      padding: 0.3rem 0.4rem;
      border: 1px solid var(--wa-color-surface-border, #ccc);
      border-radius: 0.25rem;
      max-width: 12rem;
    }
    button {
      padding: 0.4rem 0.8rem;
      border-radius: 0.375rem;
      border: 1px solid var(--wa-color-surface-border, #bbb);
      background: var(--wa-color-primary-fill, #2563eb);
      color: var(--wa-color-primary-text, white);
      font: inherit;
      cursor: pointer;
    }
    button:disabled {
      opacity: 0.5;
      cursor: not-allowed;
    }
    button.secondary {
      background: transparent;
      color: var(--wa-color-text-normal);
    }
    .status {
      margin-left: 1rem;
      color: var(--wa-color-text-quiet);
    }
    table {
      width: 100%;
      border-collapse: collapse;
      margin-top: 1rem;
      font-size: 0.85rem;
    }
    th,
    td {
      text-align: left;
      padding: 0.5rem 0.75rem;
      border-bottom: 1px solid var(--wa-color-surface-border, #eee);
      vertical-align: top;
    }
    th {
      background: var(--wa-color-surface-raised, #f7f7f7);
      font-weight: 600;
    }
    .pill {
      display: inline-block;
      padding: 0.1rem 0.5rem;
      border-radius: 999px;
      font-size: 0.75rem;
      font-weight: 600;
      text-transform: uppercase;
    }
    .pill.pending {
      background: #e0e7ff;
      color: #3730a3;
    }
    .pill.running {
      background: #dbeafe;
      color: #1e40af;
    }
    .pill.complete {
      background: #d1fae5;
      color: #065f46;
    }
    .pill.failed {
      background: #fde68a;
      color: #92400e;
    }
    .pill.shelved {
      background: #fecaca;
      color: #991b1b;
    }
    .err {
      color: var(--wa-color-danger-fill, #b91c1c);
      font-size: 0.8rem;
      max-width: 24rem;
      overflow: hidden;
      text-overflow: ellipsis;
      white-space: nowrap;
    }
    .row-actions {
      display: flex;
      gap: 0.25rem;
    }
    .row-actions button {
      padding: 0.2rem 0.5rem;
      font-size: 0.8rem;
    }
  `;

  override connectedCallback(): void {
    super.connectedCallback();
    void this.refresh();
  }

  private async refresh(): Promise<void> {
    this.busy = true;
    try {
      const [settings, rows] = await Promise.all([
        api.get<BackfillSettings>("/api/admin/channel-backfill/settings"),
        api.get<ChannelBackfillRow[]>("/api/admin/channel-sync-state"),
      ]);
      this.settings = settings;
      this.enabled = settings.enabled;
      this.notifyOnShelve = settings.notify_on_shelve;
      this.rows = rows;
    } catch (err) {
      this.status = (err as Error).message;
    } finally {
      this.busy = false;
    }
  }

  private fieldError(key: NumericKey): string | null {
    const raw = this.form[key].trim();
    if (!raw) return null;
    const n = Number(raw);
    if (!Number.isFinite(n)) return "Not a number";
    const [min, max] = RANGES[key];
    if (n < min || n > max) return `Must be ${min}..${max}`;
    return null;
  }

  private hasErrors(): boolean {
    return Object.keys(this.form).some((k) => this.fieldError(k as NumericKey) !== null);
  }

  private async save(): Promise<void> {
    if (this.hasErrors()) {
      this.status = "Fix the highlighted fields first.";
      return;
    }
    this.busy = true;
    this.status = "";
    const body: Record<string, number | boolean> = {
      enabled: this.enabled,
      notify_on_shelve: this.notifyOnShelve,
    };
    for (const key of Object.keys(this.form) as NumericKey[]) {
      const raw = this.form[key].trim();
      if (raw) body[key] = Number(raw);
    }
    try {
      this.settings = await api.put<BackfillSettings>("/api/admin/channel-backfill/settings", body);
      this.enabled = this.settings.enabled;
      this.notifyOnShelve = this.settings.notify_on_shelve;
      // Clear the inputs after a successful save so the next edit
      // starts from a clean slate.
      this.form = {
        min_gap_between_channels_s: "",
        re_backfill_interval_s: "",
        subprocess_timeout_s: "",
        ytdlp_sleep_requests_s: "",
        ytdlp_sleep_interval_s: "",
        ytdlp_max_sleep_interval_s: "",
        max_consecutive_errors_before_shelve: "",
        idle_tick_s: "",
      };
      this.status = "Saved.";
    } catch (err) {
      this.status = (err as Error).message;
    } finally {
      this.busy = false;
    }
  }

  private async runNow(channelId: string): Promise<void> {
    this.busy = true;
    try {
      await api.post(`/api/admin/channel-backfill/run-now/${encodeURIComponent(channelId)}`, {});
      this.status = "Queued.";
      await this.refresh();
    } catch (err) {
      this.status = (err as Error).message;
    } finally {
      this.busy = false;
    }
  }

  private async unshelve(channelId: string): Promise<void> {
    this.busy = true;
    try {
      await api.post(`/api/admin/channel-backfill/unshelve/${encodeURIComponent(channelId)}`, {});
      this.status = "Unshelved.";
      await this.refresh();
    } catch (err) {
      this.status = (err as Error).message;
    } finally {
      this.busy = false;
    }
  }

  private onNumericInput(key: NumericKey, ev: Event): void {
    this.form = {
      ...this.form,
      [key]: (ev.target as HTMLInputElement).value,
    };
  }

  private renderField(key: NumericKey, label: string, help?: string): unknown {
    const err = this.fieldError(key);
    const current =
      this.settings != null
        ? String((this.settings as unknown as Record<string, number>)[key])
        : "";
    const [min, max] = RANGES[key];
    return html`
      <label for="${key}">${label}</label>
      <div>
        <input
          id="${key}"
          type="number"
          min=${min}
          max=${max}
          placeholder=${current}
          .value=${this.form[key]}
          @input=${(e: Event) => this.onNumericInput(key, e)}
        />
        ${err ? html`<span class="err"> ${err}</span>` : nothing}
      </div>
      ${help ? html`<p class="help">${help}</p>` : nothing}
    `;
  }

  private formatTime(unix: number | null): string {
    if (unix == null || unix === 0) return "—";
    const d = new Date(unix * 1000);
    return d.toLocaleString();
  }

  override render() {
    if (this.settings == null) {
      return html`<p>Loading…</p>`;
    }
    return html`
      <div class="card">
        <h3>Channel archive backfill</h3>
        <p class="help" style="grid-column: 1 / -1; max-width: 50rem;">
          The backfiller runs <code>yt-dlp --flat-playlist</code> against each allowlisted channel
          on the configured cadence, writing the full upload history into the local archive.
          Single-concurrency by design; the gap between channels caps the family-wide rate on the
          anti-bot-sensitive path. Defaults are conservative — 1 channel/hour, 30-day re-backfill
          cycle.
        </p>

        <div class="grid">
          <label for="enabled">Enabled</label>
          <div>
            <input
              id="enabled"
              type="checkbox"
              .checked=${this.enabled}
              @change=${(e: Event) => (this.enabled = (e.target as HTMLInputElement).checked)}
            />
          </div>

          ${this.renderField(
            "min_gap_between_channels_s",
            "Min gap between channels (s)",
            "Sleep between consecutive backfills. 3600 = 1 channel/hour. ±15% jitter applied.",
          )}
          ${this.renderField(
            "re_backfill_interval_s",
            "Re-backfill interval (s)",
            "How often to re-run the full archive pass per channel. 2592000 = 30 days. ±5% jitter applied at completion to avoid synchronisation.",
          )}
          ${this.renderField(
            "subprocess_timeout_s",
            "Per-channel subprocess timeout (s)",
            "Hard ceiling on a single yt-dlp invocation. Very large channels (10k+ uploads) may need a few minutes.",
          )}
          ${this.renderField(
            "ytdlp_sleep_requests_s",
            "yt-dlp --sleep-requests (s)",
            "Sleep between successive InnerTube pagination requests inside the subprocess.",
          )}
          ${this.renderField(
            "ytdlp_sleep_interval_s",
            "yt-dlp --sleep-interval (s)",
            "Lower bound on yt-dlp's random sleep between requests.",
          )}
          ${this.renderField(
            "ytdlp_max_sleep_interval_s",
            "yt-dlp --max-sleep-interval (s)",
            "Upper bound on yt-dlp's random sleep between requests.",
          )}
          ${this.renderField(
            "max_consecutive_errors_before_shelve",
            "Errors before shelve",
            "After this many consecutive failures, the channel is shelved and a parent notification fires.",
          )}
          ${this.renderField(
            "idle_tick_s",
            "Idle tick (s)",
            "Sleep when no channel is due. Larger = lower CPU; smaller = faster response to settings changes.",
          )}

          <label for="notify_on_shelve">Notify on shelve</label>
          <div>
            <input
              id="notify_on_shelve"
              type="checkbox"
              .checked=${this.notifyOnShelve}
              @change=${(e: Event) =>
                (this.notifyOnShelve = (e.target as HTMLInputElement).checked)}
            />
          </div>
        </div>

        <p>
          <button
            type="button"
            @click=${() => this.save()}
            ?disabled=${this.busy || this.hasErrors()}
          >
            Save
          </button>
          <button
            type="button"
            class="secondary"
            @click=${() => this.refresh()}
            ?disabled=${this.busy}
          >
            Refresh
          </button>
          <span class="status">${this.status}</span>
        </p>
      </div>

      <div class="card" style="margin-top: 1rem;">
        <h3>Per-channel state</h3>
        ${this.rows.length === 0
          ? html`<p>No allowlisted channels.</p>`
          : html`
              <table>
                <thead>
                  <tr>
                    <th>Channel</th>
                    <th>Status</th>
                    <th>Videos</th>
                    <th>Last completed</th>
                    <th>Last error</th>
                    <th>Next due</th>
                    <th></th>
                  </tr>
                </thead>
                <tbody>
                  ${this.rows.map(
                    (r) => html`
                      <tr>
                        <td>
                          <div>${r.channel_title ?? r.channel_id}</div>
                          <div class="help" style="font-size: 0.75rem;">${r.channel_id}</div>
                        </td>
                        <td>
                          <span class="pill ${r.backfill_status}"> ${r.backfill_status} </span>
                          ${r.backfill_consecutive_errors > 0
                            ? html` <span class="help"
                                >(${r.backfill_consecutive_errors} err)</span
                              >`
                            : nothing}
                        </td>
                        <td>
                          ${r.item_count}
                          ${r.archived_count > 0
                            ? html`<span class="help"> (+${r.archived_count} archived)</span>`
                            : nothing}
                        </td>
                        <td>${this.formatTime(r.backfill_last_completed_at)}</td>
                        <td class="err" title=${r.backfill_last_error ?? ""}>
                          ${r.backfill_last_error ?? "—"}
                        </td>
                        <td>${this.formatTime(r.backfill_next_at)}</td>
                        <td class="row-actions">
                          <button
                            type="button"
                            class="secondary"
                            @click=${() => this.runNow(r.channel_id)}
                            ?disabled=${this.busy || r.backfill_status === "running"}
                            title="Bump to front of queue"
                          >
                            Run now
                          </button>
                          ${r.backfill_status === "shelved"
                            ? html`
                                <button
                                  type="button"
                                  @click=${() => this.unshelve(r.channel_id)}
                                  ?disabled=${this.busy}
                                  title="Clear shelved state and retry"
                                >
                                  Unshelve
                                </button>
                              `
                            : nothing}
                        </td>
                      </tr>
                    `,
                  )}
                </tbody>
              </table>
            `}
      </div>
    `;
  }
}

declare global {
  interface HTMLElementTagNameMap {
    "hometube-channel-backfill-settings": ChannelBackfillSettings;
  }
}
