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
  sidecar_fallback_enabled: boolean;
  sidecar_fallback_min_interval_s: number;
  sidecar_fallback_max_per_hour: number;
  raw?: {
    dispatch_delay_ms: string | null;
    max_inflight: string | null;
    batch_size: string | null;
    idle_tick_s: string | null;
    channel_interval_s: string | null;
    sidecar_fallback_enabled: string | null;
    sidecar_fallback_min_interval_s: string | null;
    sidecar_fallback_max_per_hour: string | null;
  };
}

/** Numeric tunables that have a [min, max] range. The boolean
 *  `sidecar_fallback_enabled` is handled separately. */
type NumericKey =
  | "dispatch_delay_ms"
  | "max_inflight"
  | "batch_size"
  | "idle_tick_s"
  | "channel_interval_s"
  | "sidecar_fallback_min_interval_s"
  | "sidecar_fallback_max_per_hour";

// Per-field range limits. Keep in sync with the backend's range
// validation in `admin_put_refresher_settings` and `RefresherConfig::load`.
const RANGES: Record<NumericKey, [number, number]> = {
  dispatch_delay_ms: [50, 600_000],
  max_inflight: [1, 64],
  batch_size: [1, 500],
  idle_tick_s: [1, 3600],
  channel_interval_s: [60, 86_400],
  sidecar_fallback_min_interval_s: [60, 86_400],
  sidecar_fallback_max_per_hour: [0, 10_000],
};

/**
 * One row of `/api/admin/channel-sync-state`. Renamed from the legacy
 * `FeedSourceStatus` when migration 020 consolidated `feed_sources`
 * into `channel_sync_state` and dropped the polymorphic `kind` /
 * `source_id` columns — every source is an RSS channel now.
 *
 * Only the freshness-tier (`rss_*`) fields and `item_count` /
 * `last_sidecar_fallback_at` are surfaced here; the backfill-tier
 * fields on the same row are owned by `<hometube-channel-backfill-settings>`.
 */
interface ChannelSyncStateStatus {
  channel_id: string;
  channel_title: string | null;
  rss_last_polled_at: number | null;
  rss_last_success_at: number | null;
  rss_last_error: string | null;
  rss_consecutive_errors: number;
  rss_next_poll_at: number;
  item_count: number;
  last_sidecar_fallback_at: number | null;
}

interface RefresherCapacity {
  total_sources: number;
  queue_depth: number;
  polls_last_hour: number;
  sidecar_fallbacks_last_hour: number;
  theoretical_polls_per_hour: number;
  required_polls_per_hour: number;
  utilization_pct: number;
}

@customElement("hometube-feed-refresher-settings")
export class FeedRefresherSettings extends LitElement {
  @state() private settings: RefresherSettings | null = null;
  @state() private capacity: RefresherCapacity | null = null;
  @state() private sources: ChannelSyncStateStatus[] = [];
  @state() private busy = false;
  @state() private status = "";

  // Editable copies of the numeric inputs. We keep them as strings
  // because <input type="number"> with the `min` / `max` attributes
  // still allows empty intermediate values during typing.
  @state() private form: Record<NumericKey, string> = {
    dispatch_delay_ms: "",
    max_inflight: "",
    batch_size: "",
    idle_tick_s: "",
    channel_interval_s: "",
    sidecar_fallback_min_interval_s: "",
    sidecar_fallback_max_per_hour: "",
  };

  /** The fallback toggle is a checkbox, not a number, so it lives
   *  outside `form`. Mirrors `settings.sidecar_fallback_enabled`
   *  until the user saves. */
  @state() private fallbackEnabled = true;

  /** Range-check a field against `RANGES`; empty strings count as
   * "leave unchanged" and are valid. */
  private fieldError(key: NumericKey): string | null {
    const raw = this.form[key].trim();
    if (!raw) return null;
    const num = Number(raw);
    if (!Number.isFinite(num)) return "must be a number";
    const [min, max] = RANGES[key];
    if (num < min || num > max) return `must be ${min}\u2013${max}`;
    return null;
  }

  private hasAnyError(): boolean {
    return (Object.keys(RANGES) as NumericKey[]).some((k) => this.fieldError(k) !== null);
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
    .capacity {
      margin-bottom: 1.25rem;
      padding-bottom: 1rem;
      border-bottom: 1px solid var(--wa-color-surface-border);
    }
    .capacity h3 {
      margin: 0 0 0.25rem;
      font-size: 1rem;
    }
    .capacity-grid {
      display: grid;
      grid-template-columns: repeat(auto-fit, minmax(10rem, 1fr));
      gap: 0.5rem;
      margin-top: 0.75rem;
    }
    .metric {
      padding: 0.6rem 0.8rem;
      border: 1px solid var(--wa-color-surface-border);
      border-radius: 0.375rem;
      background: var(--wa-color-surface-default);
    }
    .metric-value {
      font-size: 1.3rem;
      font-weight: 700;
      line-height: 1.2;
    }
    .metric-label {
      font-size: 0.85rem;
      color: var(--wa-color-text-quiet);
      text-transform: uppercase;
      letter-spacing: 0.04em;
    }
    .metric-ok .metric-value {
      color: var(--wa-color-success-fill, #15803d);
    }
    .metric-warn .metric-value {
      color: var(--wa-color-warning-fill, #b45309);
    }
    .metric-danger .metric-value {
      color: var(--wa-color-danger-fill, #b91c1c);
    }
    .toggle {
      display: flex;
      align-items: flex-start;
      gap: 0.6rem;
      padding: 0.5rem 0;
    }
    .toggle input[type="checkbox"] {
      margin-top: 0.2rem;
      width: 1rem;
      height: 1rem;
    }
    .toggle-body {
      display: flex;
      flex-direction: column;
      gap: 0.15rem;
    }
    .toggle-body strong {
      font-weight: 600;
      font-size: 0.9rem;
    }
    .section-heading {
      margin: 1.25rem 0 0.5rem;
      font-size: 1rem;
    }
  `;

  override connectedCallback(): void {
    super.connectedCallback();
    void this.load();
  }

  private async load(): Promise<void> {
    try {
      const [settings, capacity, sources] = await Promise.all([
        api.get<RefresherSettings>("/api/admin/feed-refresher/settings"),
        api.get<RefresherCapacity>("/api/admin/feed-refresher/capacity"),
        api.get<ChannelSyncStateStatus[]>("/api/admin/channel-sync-state"),
      ]);
      this.settings = settings;
      this.capacity = capacity;
      this.sources = sources;
      this.fallbackEnabled = settings.sidecar_fallback_enabled;
      this.form = {
        dispatch_delay_ms: String(settings.dispatch_delay_ms),
        max_inflight: String(settings.max_inflight),
        batch_size: String(settings.batch_size),
        idle_tick_s: String(settings.idle_tick_s),
        channel_interval_s: String(settings.channel_interval_s),
        sidecar_fallback_min_interval_s: String(settings.sidecar_fallback_min_interval_s),
        sidecar_fallback_max_per_hour: String(settings.sidecar_fallback_max_per_hour),
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
    const payload: Record<string, number | boolean> = {};
    for (const key of Object.keys(this.form) as NumericKey[]) {
      const raw = this.form[key].trim();
      if (!raw) continue;
      payload[key] = Number(raw);
    }
    // Always send the fallback flag — sending it unchanged just
    // rewrites the same `app_config` row, which is harmless.
    payload.sidecar_fallback_enabled = this.fallbackEnabled;
    this.busy = true;
    try {
      this.settings = await api.put<RefresherSettings>(
        "/api/admin/feed-refresher/settings",
        payload,
      );
      // Refresh capacity too — utilisation depends on dispatch_delay
      // and channel_interval, so a save can move the needle.
      this.capacity = await api.get<RefresherCapacity>("/api/admin/feed-refresher/capacity");
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

  private updateField(key: NumericKey, ev: Event): void {
    const value = (ev.target as HTMLInputElement).value;
    this.form = { ...this.form, [key]: value };
  }

  /** Returns a warning string when the DB value disagrees with the
   * effective (clamped) value — meaning the stored value was rejected
   * by range validation in the backend. */
  private rawWarning(key: NumericKey, effective: number | string): string | null {
    const raw = this.settings?.raw?.[key];
    if (raw == null) return null;
    if (raw === String(effective)) return null;
    return `Stored value "${raw}" is out of range; using ${effective} instead.`;
  }

  private renderField(key: NumericKey, label: string, effective: number, hint: string) {
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

  private renderCapacity() {
    if (!this.capacity) return nothing;
    const c = this.capacity;
    // Threshold tiers: <70% = ok, 70-95% = warn, >=95% = danger.
    const tier = c.utilization_pct < 70 ? "ok" : c.utilization_pct < 95 ? "warn" : "danger";
    const queueTier = c.queue_depth === 0 ? "ok" : c.queue_depth < 5 ? "warn" : "danger";
    return html`
      <section class="capacity">
        <h3>Capacity</h3>
        <p class="hint">
          A quick "is the refresher keeping up?" check. Utilisation above ~70% means the dispatcher
          is approaching saturation — lower
          <code>Dispatch delay</code>
          or raise <code>Channel interval</code> before it climbs further. A non-zero
          <strong>Queue depth</strong> persisting between page refreshes means the dispatcher can't
          drain its backlog at current settings.
        </p>
        <div class="capacity-grid">
          <div class="metric metric-${tier}">
            <div class="metric-value">${c.utilization_pct.toFixed(0)}%</div>
            <div class="metric-label">Utilisation</div>
            <div class="hint">
              Need ${c.required_polls_per_hour.toFixed(0)} of ${c.theoretical_polls_per_hour}
              polls/hr
            </div>
          </div>
          <div class="metric metric-${queueTier}">
            <div class="metric-value">${c.queue_depth}</div>
            <div class="metric-label">Queue depth</div>
            <div class="hint">Sources past <code>next_poll_at</code></div>
          </div>
          <div class="metric">
            <div class="metric-value">${c.polls_last_hour}</div>
            <div class="metric-label">Polls (1h)</div>
            <div class="hint">Distinct sources polled</div>
          </div>
          <div class="metric">
            <div class="metric-value">${c.sidecar_fallbacks_last_hour}</div>
            <div class="metric-label">Sidecar fallbacks (1h)</div>
            <div class="hint">
              Cap:
              ${this.settings?.sidecar_fallback_max_per_hour
                ? this.settings.sidecar_fallback_max_per_hour
                : "∞"}
            </div>
          </div>
          <div class="metric">
            <div class="metric-value">${c.total_sources}</div>
            <div class="metric-label">Total sources</div>
            <div class="hint">Allowlisted channels</div>
          </div>
        </div>
      </section>
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
          so <code>/api/feed/new-videos</code> is a single database read. Tunables are read live
          from <code>app_config</code>; changes apply within
          <strong>${this.settings.idle_tick_s} s</strong>.
        </p>

        ${this.renderCapacity()}

        <h3 class="section-heading">RSS poll cadence</h3>
        <p class="hint">
          These five knobs control the steady-state RSS poller. The defaults handle up to a few
          hundred channels comfortably. Lower <code>Dispatch delay</code> or raise
          <code>Channel interval</code> as you scale; touch the others only if you have a specific
          reason.
        </p>
        <div class="grid">
          ${this.renderField(
            "dispatch_delay_ms",
            "Dispatch delay (ms)",
            this.settings.dispatch_delay_ms,
            "Minimum gap between successive RSS polls. Caps the global request rate at ~1 / (delay) per second. Lower this when the capacity panel shows utilisation above 70%; the lowest reasonable value for a residential IP is around 250–500 ms (faster than that risks YouTube RSS soft-throttling).",
          )}
          ${this.renderField(
            "channel_interval_s",
            "Channel interval (s)",
            this.settings.channel_interval_s,
            "Steady-state interval between successful polls of the same channel. Default 3600 (1h). Raise this when scaling past ~1000 sources and you'd rather accept staler feeds than tune the dispatcher; halve it (1800) when freshness matters more than request rate.",
          )}
          ${this.renderField(
            "max_inflight",
            "Max inflight",
            this.settings.max_inflight,
            "Maximum concurrent RSS requests in flight. Default 4. Raise (toward 16) only when network latency to YouTube is high and the dispatch delay alone leaves the dispatcher idle; lower (toward 2) if you're concerned about burstiness on a low-bandwidth uplink.",
          )}
          ${this.renderField(
            "batch_size",
            "Batch size",
            this.settings.batch_size,
            "Sources claimed from the queue per loop iteration. Default 25. Increase for very large allowlists (1000+) to reduce SQL overhead; decrease only if you observe lock contention with other readers — uncommon.",
          )}
          ${this.renderField(
            "idle_tick_s",
            "Idle tick (s)",
            this.settings.idle_tick_s,
            "How long to sleep when there's nothing to do. Default 30. Lower this for snappier reaction after adding a new allowlist entry; raise it to reduce CPU/database wakeups on very small installs.",
          )}
        </div>

        <h3 class="section-heading">Sidecar fallback (RSS outage resilience)</h3>
        <p class="hint">
          When an RSS poll fails — most commonly during YouTube's intermittent RSS outages — the
          refresher can fall back to the youtubei.js discovery sidecar to keep the feed fresh and to
          classify whether the source is actually dead (sidecar 404) vs. temporarily unreachable.
          Sidecar calls hit the InnerTube <code>browse</code> endpoint, which has a generous but
          real per-IP rate limit, so the two caps below protect against runaway fallback traffic
          during long outages.
        </p>
        <label class="toggle">
          <input
            type="checkbox"
            .checked=${this.fallbackEnabled}
            @change=${(e: Event) => (this.fallbackEnabled = (e.target as HTMLInputElement).checked)}
          />
          <div class="toggle-body">
            <strong>Enable sidecar fallback</strong>
            <span class="hint">
              The kill switch. Leave on by default — at small scale the fallback is essentially
              free. Disable if you observe sidecar errors during a YouTube anti-bot tightening and
              want to halt automated InnerTube traffic without restarting the process.
            </span>
          </div>
        </label>
        <div class="grid">
          ${this.renderField(
            "sidecar_fallback_min_interval_s",
            "Per-source min interval (s)",
            this.settings.sidecar_fallback_min_interval_s,
            "Minimum seconds between successive sidecar fallbacks for the same source. Default 3600 (1h). Lower this (toward 600) if you want fresher data during outages and have few sources; raise it (toward 14400 = 4h) if you have thousands of sources and want to cap automated InnerTube load.",
          )}
          ${this.renderField(
            "sidecar_fallback_max_per_hour",
            "Aggregate cap (per hour)",
            this.settings.sidecar_fallback_max_per_hour,
            "Maximum total sidecar fallbacks per hour across all sources. Default 120. Set to 0 for unlimited. Useful at scale: at 1000+ sources during a sustained outage, the per-source cap alone could still allow ~1000 calls/hour — this aggregate cap reins that in without disabling fallback entirely.",
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
                    <th>Last fallback</th>
                    <th>Next poll</th>
                    <th>Status</th>
                  </tr>
                </thead>
                <tbody>
                  ${this.sources.map(
                    (s) => html`
                      <tr>
                        <td>
                          <div>${s.channel_title ?? s.channel_id}</div>
                          <div class="hint"><code>${s.channel_id}</code></div>
                        </td>
                        <td>${s.item_count}</td>
                        <td>${this.fmtTimestamp(s.rss_last_polled_at)}</td>
                        <td>${this.fmtTimestamp(s.rss_last_success_at)}</td>
                        <td>${this.fmtTimestamp(s.last_sidecar_fallback_at)}</td>
                        <td>${this.fmtTimestamp(s.rss_next_poll_at)}</td>
                        <td>
                          ${s.rss_last_error
                            ? html`<span class="error" title=${s.rss_last_error}>
                                ${s.rss_consecutive_errors}
                                error${s.rss_consecutive_errors === 1 ? "" : "s"}
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
