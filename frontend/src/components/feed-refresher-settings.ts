/**
 * <hometube-feed-refresher-settings>
 *
 * Parent-only control panel for the background feed refresher.
 *
 * The refresher polls YouTube channel RSS feeds on a schedule and
 * writes results into `feed_source_items`, which backs the child
 * `/child/home` "New Videos" row. The endpoint is read live by the
 * refresher loop, so changes take effect within `idle_tick_s` seconds
 * without a process restart.
 *
 * Also surfaces a read-only diagnostics table of per-source health:
 * last poll time, last error, consecutive error count, next scheduled
 * poll, and the number of items currently held for the source.
 */

import { LitElement, css, html, nothing } from "lit";
import { customElement, state } from "lit/decorators.js";

import { api } from "../services/api.js";

interface RefresherSettings {
  dispatch_delay_ms: number;
  max_inflight: number;
  batch_size: number;
  idle_tick_s: number;
  channel_interval_s: number;
  raw?: {
    dispatch_delay_ms: string | null;
    max_inflight: string | null;
    batch_size: string | null;
    idle_tick_s: string | null;
    channel_interval_s: string | null;
  };
}

// Per-field range limits. Keep in sync with the backend's range
// validation in `admin_put_refresher_settings` and `RefresherConfig::load`.
const RANGES: Record<keyof Omit<RefresherSettings, "raw">, [number, number]> = {
  dispatch_delay_ms: [50, 600_000],
  max_inflight: [1, 64],
  batch_size: [1, 500],
  idle_tick_s: [1, 3600],
  channel_interval_s: [60, 86_400],
};

interface FeedSourceStatus {
  kind: string;
  source_id: string;
  title: string | null;
  last_polled_at: number | null;
  last_success_at: number | null;
  last_error: string | null;
  consecutive_errors: number;
  next_poll_at: number;
  item_count: number;
}

@customElement("hometube-feed-refresher-settings")
export class FeedRefresherSettings extends LitElement {
  @state() private settings: RefresherSettings | null = null;
  @state() private sources: FeedSourceStatus[] = [];
  @state() private busy = false;
  @state() private status = "";

  // Editable copies of the numeric inputs. We keep them as strings
  // because <input type="number"> with the `min` / `max` attributes
  // still allows empty intermediate values during typing.
  @state() private form: Record<keyof Omit<RefresherSettings, "raw">, string> = {
    dispatch_delay_ms: "",
    max_inflight: "",
    batch_size: "",
    idle_tick_s: "",
    channel_interval_s: "",
  };

  /** Range-check a field against `RANGES`; empty strings count as
   * "leave unchanged" and are valid. */
  private fieldError(key: keyof typeof RANGES): string | null {
    const raw = this.form[key].trim();
    if (!raw) return null;
    const num = Number(raw);
    if (!Number.isFinite(num)) return "must be a number";
    const [min, max] = RANGES[key];
    if (num < min || num > max) return `must be ${min}\u2013${max}`;
    return null;
  }

  private hasAnyError(): boolean {
    return (Object.keys(RANGES) as Array<keyof typeof RANGES>).some(
      (k) => this.fieldError(k) !== null,
    );
  }

  static styles = css`
    :host {
      display: block;
    }
    article {
      padding: 1rem;
      border: 1px solid var(--wa-color-surface-border);
      border-radius: 0.5rem;
      background: var(--wa-color-surface-default);
    }
    .grid {
      display: grid;
      grid-template-columns: repeat(auto-fit, minmax(14rem, 1fr));
      gap: 0.75rem 1rem;
      margin: 0.5rem 0 1rem;
    }
    label {
      display: flex;
      flex-direction: column;
      gap: 0.25rem;
      font-weight: 600;
      font-size: 0.9rem;
    }
    .hint {
      font-weight: 400;
      font-size: 0.8rem;
      color: var(--wa-color-text-quiet);
    }
    input[type="number"] {
      padding: 0.4rem 0.6rem;
      border-radius: 0.375rem;
      border: 1px solid var(--wa-color-surface-border, #ccc);
      background: var(--wa-color-surface-default);
      color: var(--wa-color-text-normal);
      font: inherit;
    }
    button {
      padding: 0.4rem 0.75rem;
      border-radius: 0.375rem;
      border: 1px solid var(--wa-color-surface-border, #ccc);
      background: var(--wa-color-brand-fill, #2563eb);
      color: white;
      font: inherit;
      cursor: pointer;
    }
    button[disabled] {
      opacity: 0.6;
      cursor: progress;
    }
    table {
      width: 100%;
      border-collapse: collapse;
      margin-top: 0.5rem;
      font-size: 0.85rem;
    }
    th,
    td {
      text-align: left;
      padding: 0.4rem 0.5rem;
      border-bottom: 1px solid var(--wa-color-surface-border);
      vertical-align: top;
    }
    th {
      color: var(--wa-color-text-quiet);
      font-size: 0.8rem;
    }
    .error {
      color: var(--wa-color-danger-fill, #b91c1c);
      word-break: break-word;
    }
    .ok {
      color: var(--wa-color-success-fill, #15803d);
    }
    .sources-heading {
      margin: 1.5rem 0 0.25rem;
      font-size: 1rem;
    }
    .status-text {
      margin-top: 0.5rem;
      font-size: 0.9rem;
      color: var(--wa-color-text-quiet);
    }
  `;

  override connectedCallback(): void {
    super.connectedCallback();
    void this.load();
  }

  private async load(): Promise<void> {
    try {
      const [settings, sources] = await Promise.all([
        api.get<RefresherSettings>("/api/admin/feed-refresher/settings"),
        api.get<FeedSourceStatus[]>("/api/admin/feed-sources"),
      ]);
      this.settings = settings;
      this.sources = sources;
      this.form = {
        dispatch_delay_ms: String(settings.dispatch_delay_ms),
        max_inflight: String(settings.max_inflight),
        batch_size: String(settings.batch_size),
        idle_tick_s: String(settings.idle_tick_s),
        channel_interval_s: String(settings.channel_interval_s),
      };
    } catch (err) {
      this.status = `Failed to load: ${(err as Error).message}`;
    }
  }

  private async save(): Promise<void> {
    if (this.hasAnyError()) {
      this.status = "Fix the highlighted fields before saving.";
      return;
    }
    const payload: Record<string, number> = {};
    for (const key of Object.keys(this.form) as Array<keyof typeof RANGES>) {
      const raw = this.form[key].trim();
      if (!raw) continue;
      payload[key] = Number(raw);
    }
    this.busy = true;
    try {
      this.settings = await api.put<RefresherSettings>(
        "/api/admin/feed-refresher/settings",
        payload,
      );
      this.status = "Saved. Changes take effect on the next refresher tick.";
    } catch (err) {
      this.status = `Save failed: ${(err as Error).message}`;
    } finally {
      this.busy = false;
    }
  }

  private fmtTimestamp(ts: number | null): string {
    if (ts === null || ts === 0) return "—";
    return new Date(ts * 1000).toLocaleString();
  }

  private updateField(key: keyof RefresherSettings, ev: Event): void {
    const value = (ev.target as HTMLInputElement).value;
    this.form = { ...this.form, [key]: value };
  }

  /** Returns a warning string when the DB value disagrees with the
   * effective (clamped) value — meaning the stored value was rejected
   * by range validation in the backend. */
  private rawWarning(key: keyof typeof RANGES, effective: number | string): string | null {
    const raw = this.settings?.raw?.[key];
    if (raw == null) return null;
    if (raw === String(effective)) return null;
    return `Stored value "${raw}" is out of range; using ${effective} instead.`;
  }

  private renderField(key: keyof typeof RANGES, label: string, effective: number, hint: string) {
    const [min, max] = RANGES[key];
    const err = this.fieldError(key);
    const warn = this.rawWarning(key, effective);
    return html`
      <label>
        ${label}
        <input
          type="number"
          min=${min}
          max=${max}
          .value=${this.form[key]}
          aria-invalid=${err !== null}
          @input=${(e: Event) => this.updateField(key, e)}
        />
        <span class="hint">${hint}</span>
        ${err ? html`<span class="error">${err}</span>` : nothing}
        ${warn ? html`<span class="error">${warn}</span>` : nothing}
      </label>
    `;
  }

  override render() {
    if (!this.settings) {
      return html`<article>Loading…</article>`;
    }
    return html`
      <article>
        <p class="hint">
          The background refresher polls each allowlisted channel's RSS feed and caches the results
          so <code>/api/feed/new-videos</code>
          is a single database read. Tunables are read live from
          <code>app_config</code>; changes apply within
          <strong>${this.settings.idle_tick_s} s</strong>.
        </p>

        <div class="grid">
          ${this.renderField(
            "dispatch_delay_ms",
            "Dispatch delay (ms)",
            this.settings.dispatch_delay_ms,
            "Minimum gap between successive RSS polls. Caps the global request rate at ~1 / (delay) per second.",
          )}
          ${this.renderField(
            "max_inflight",
            "Max inflight",
            this.settings.max_inflight,
            "Maximum concurrent RSS requests across all sources.",
          )}
          ${this.renderField(
            "batch_size",
            "Batch size",
            this.settings.batch_size,
            "Overdue sources claimed per loop iteration.",
          )}
          ${this.renderField(
            "idle_tick_s",
            "Idle tick (s)",
            this.settings.idle_tick_s,
            "Sleep duration when no sources are overdue.",
          )}
          ${this.renderField(
            "channel_interval_s",
            "Channel interval (s)",
            this.settings.channel_interval_s,
            "Steady-state interval between successful polls of the same channel.",
          )}
        </div>

        <button @click=${() => this.save()} ?disabled=${this.busy || this.hasAnyError()}>
          ${this.busy ? "Saving…" : "Save settings"}
        </button>
        ${this.status ? html`<p class="status-text" role="status">${this.status}</p>` : nothing}

        <h3 class="sources-heading">Feed sources (${this.sources.length})</h3>
        ${this.sources.length === 0
          ? html`<p class="hint">No sources registered yet. Allowlist a channel to add one.</p>`
          : html`
              <table>
                <thead>
                  <tr>
                    <th>Source</th>
                    <th>Items</th>
                    <th>Last polled</th>
                    <th>Last success</th>
                    <th>Next poll</th>
                    <th>Status</th>
                  </tr>
                </thead>
                <tbody>
                  ${this.sources.map(
                    (s) => html`
                      <tr>
                        <td>
                          <div>${s.title ?? s.source_id}</div>
                          <div class="hint">${s.kind}: <code>${s.source_id}</code></div>
                        </td>
                        <td>${s.item_count}</td>
                        <td>${this.fmtTimestamp(s.last_polled_at)}</td>
                        <td>${this.fmtTimestamp(s.last_success_at)}</td>
                        <td>${this.fmtTimestamp(s.next_poll_at)}</td>
                        <td>
                          ${s.last_error
                            ? html`<span class="error" title=${s.last_error}>
                                ${s.consecutive_errors}
                                error${s.consecutive_errors === 1 ? "" : "s"}
                              </span>`
                            : html`<span class="ok">OK</span>`}
                        </td>
                      </tr>
                    `,
                  )}
                </tbody>
              </table>
            `}
      </article>
    `;
  }
}
