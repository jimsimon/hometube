/**
 * <hometube-activity-dashboard child-id="...">
 *
 * Page-level component for /parent/activity. Composes:
 *
 *   - summary cards (today / this week / this month)
 *   - <hometube-activity-chart> with the last-30-days daily totals
 *   - top-channels list
 *   - paginated detailed history table
 *   - paginated search log
 *
 * The `child-id` attribute is set externally by the parent nav's
 * `child-changed` event wiring on the activity page template.
 */

import { LitElement, html, css, nothing } from "lit";
import { customElement, property, state } from "lit/decorators.js";

import { ApiError, api } from "../services/api.js";
import type {
  ActivityHistoryEntry,
  ActivitySummary,
  SearchLogEntry,
  TopChannel,
} from "../types/index.js";

import "./activity-chart.js";
import "./loading-spinner.js";
import "./error-banner.js";

const HISTORY_PAGE_SIZE = 25;
const SEARCH_PAGE_SIZE = 25;

@customElement("hometube-activity-dashboard")
export class ActivityDashboard extends LitElement {
  @property({ type: Number, attribute: "child-id" })
  childId: number | null = null;

  @state() private today: ActivitySummary | null = null;
  @state() private week: ActivitySummary | null = null;
  @state() private month: ActivitySummary | null = null;
  @state() private topChannels: TopChannel[] = [];
  @state() private history: ActivityHistoryEntry[] = [];
  @state() private searchLog: SearchLogEntry[] = [];

  @state() private loading = false;
  @state() private error = "";

  static styles = css`
    :host {
      display: block;
    }
    .summary-cards {
      display: grid;
      gap: 1rem;
      grid-template-columns: repeat(auto-fill, minmax(min(12rem, 100%), 1fr));
      margin-block: 1rem;
    }
    .card {
      padding: 1rem;
      border: 1px solid var(--wa-color-surface-border);
      border-radius: 0.5rem;
      background: var(--wa-color-surface-default);
    }
    .card .label {
      font-size: 0.85rem;
      color: var(--wa-color-text-quiet);
    }
    .card .value {
      font-size: 1.5rem;
      font-weight: 700;
    }
    h2 {
      margin: 1.5rem 0 0.5rem;
      font-size: 1.1rem;
    }
    table {
      width: 100%;
      border-collapse: collapse;
      font-size: 0.95rem;
    }
    th,
    td {
      text-align: left;
      padding: 0.5rem;
      border-bottom: 1px solid var(--wa-color-surface-border);
    }
    th {
      font-weight: 600;
      color: var(--wa-color-text-quiet);
    }
    .empty {
      color: var(--wa-color-text-quiet);
      font-style: italic;
    }
    button {
      margin-top: 0.75rem;
      padding: 0.5rem 1rem;
      border-radius: 0.375rem;
      border: 1px solid var(--wa-color-surface-border);
      background: transparent;
      color: var(--wa-color-text-normal);
      font: inherit;
      cursor: pointer;
    }
  `;

  override updated(changed: Map<string, unknown>): void {
    if (changed.has("childId") && this.childId != null) {
      void this.loadAll();
    }
  }

  private async loadAll(): Promise<void> {
    if (this.childId == null) return;
    this.loading = true;
    this.error = "";
    try {
      const [today, week, month, top, history, searchLog] = await Promise.all([
        api.get<ActivitySummary>(`/api/children/${this.childId}/activity/summary?period=day`),
        api.get<ActivitySummary>(`/api/children/${this.childId}/activity/summary?period=week`),
        api.get<ActivitySummary>(`/api/children/${this.childId}/activity/summary?period=month`),
        api.get<TopChannel[]>(`/api/children/${this.childId}/activity/top-channels?period=month`),
        api.get<ActivityHistoryEntry[]>(
          `/api/children/${this.childId}/activity/history?limit=${HISTORY_PAGE_SIZE}`,
        ),
        api.get<SearchLogEntry[]>(
          `/api/children/${this.childId}/activity/search-log?limit=${SEARCH_PAGE_SIZE}`,
        ),
      ]);
      this.today = today;
      this.week = week;
      this.month = month;
      this.topChannels = top;
      this.history = history;
      this.searchLog = searchLog;
    } catch (err) {
      this.error = err instanceof ApiError ? String(err.body) : (err as Error).message;
    } finally {
      this.loading = false;
    }
  }

  private async loadMoreHistory(): Promise<void> {
    if (this.childId == null || this.history.length === 0) return;
    const last = this.history[this.history.length - 1];
    if (!last) return;
    try {
      const more = await api.get<ActivityHistoryEntry[]>(
        `/api/children/${this.childId}/activity/history?limit=${HISTORY_PAGE_SIZE}&before=${last.started_at}`,
      );
      this.history = [...this.history, ...more];
    } catch (err) {
      this.error = (err as Error).message;
    }
  }

  private async loadMoreSearch(): Promise<void> {
    if (this.childId == null || this.searchLog.length === 0) return;
    const last = this.searchLog[this.searchLog.length - 1];
    if (!last) return;
    try {
      const more = await api.get<SearchLogEntry[]>(
        `/api/children/${this.childId}/activity/search-log?limit=${SEARCH_PAGE_SIZE}&before=${last.searched_at}`,
      );
      this.searchLog = [...this.searchLog, ...more];
    } catch (err) {
      this.error = (err as Error).message;
    }
  }

  private formatMinutes(seconds: number): string {
    const minutes = Math.round(seconds / 60);
    if (minutes < 60) return `${minutes} min`;
    const hours = Math.floor(minutes / 60);
    const remainder = minutes % 60;
    return remainder === 0 ? `${hours}h` : `${hours}h ${remainder}m`;
  }

  private formatDate(unix: number): string {
    return new Date(unix * 1000).toLocaleString();
  }

  override render() {
    if (this.childId == null) {
      return html`<p class="empty">Pick a child from the menu above to see activity.</p>`;
    }
    if (this.loading && this.today == null) {
      return html`<hometube-loading-spinner label="Loading activity…"></hometube-loading-spinner>`;
    }

    return html`
      ${this.error
        ? html`<hometube-error-banner
            message=${this.error}
            dismissible
            @hometube:error-dismiss=${() => (this.error = "")}
          ></hometube-error-banner>`
        : nothing}

      <div class="summary-cards" role="region" aria-label="Activity summary">
        ${this.summaryCard("Today", this.today)} ${this.summaryCard("This week", this.week)}
        ${this.summaryCard("This month", this.month)}
      </div>

      <h2>Last 30 days</h2>
      <hometube-activity-chart
        data=${JSON.stringify(this.month?.daily_minutes ?? [])}
      ></hometube-activity-chart>

      <h2>Top channels (this month)</h2>
      ${this.topChannels.length === 0
        ? html`<p class="empty">No watched channels yet.</p>`
        : html`
            <table aria-label="Top channels by watch time">
              <thead>
                <tr>
                  <th scope="col">Channel</th>
                  <th scope="col">Time</th>
                  <th scope="col">Videos</th>
                </tr>
              </thead>
              <tbody>
                ${this.topChannels.map(
                  (c) => html`
                    <tr>
                      <td>${c.channel_title ?? "Unknown"}</td>
                      <td>${this.formatMinutes(c.total_seconds)}</td>
                      <td>${c.videos_watched}</td>
                    </tr>
                  `,
                )}
              </tbody>
            </table>
          `}

      <h2>Recent history</h2>
      ${this.history.length === 0
        ? html`<p class="empty">No watch history yet.</p>`
        : html`
            <table aria-label="Recent watch history">
              <thead>
                <tr>
                  <th scope="col">When</th>
                  <th scope="col">Video</th>
                  <th scope="col">Channel</th>
                  <th scope="col">Duration</th>
                </tr>
              </thead>
              <tbody>
                ${this.history.map(
                  (h) => html`
                    <tr>
                      <td>${this.formatDate(h.started_at)}</td>
                      <td>${h.video_title ?? h.video_id}</td>
                      <td>${h.channel_title ?? "—"}</td>
                      <td>
                        ${h.duration_seconds != null ? this.formatMinutes(h.duration_seconds) : "—"}
                      </td>
                    </tr>
                  `,
                )}
              </tbody>
            </table>
            ${this.history.length >= HISTORY_PAGE_SIZE
              ? html`<button type="button" @click=${() => void this.loadMoreHistory()}>
                  Load more history
                </button>`
              : nothing}
          `}

      <h2>Search log</h2>
      ${this.searchLog.length === 0
        ? html`<p class="empty">No searches yet.</p>`
        : html`
            <table aria-label="Search log">
              <thead>
                <tr>
                  <th scope="col">When</th>
                  <th scope="col">Query</th>
                  <th scope="col">Results</th>
                </tr>
              </thead>
              <tbody>
                ${this.searchLog.map(
                  (s) => html`
                    <tr>
                      <td>${this.formatDate(s.searched_at)}</td>
                      <td>${s.query}</td>
                      <td>${s.result_count}</td>
                    </tr>
                  `,
                )}
              </tbody>
            </table>
            ${this.searchLog.length >= SEARCH_PAGE_SIZE
              ? html`<button type="button" @click=${() => void this.loadMoreSearch()}>
                  Load more searches
                </button>`
              : nothing}
          `}
    `;
  }

  private summaryCard(label: string, summary: ActivitySummary | null) {
    return html`
      <div class="card">
        <div class="label">${label}</div>
        <div class="value">${summary ? this.formatMinutes(summary.total_seconds) : "—"}</div>
        <div class="label">
          ${summary?.videos_watched ?? 0} videos · ${summary?.sessions ?? 0} sessions
        </div>
      </div>
    `;
  }
}

declare global {
  interface HTMLElementTagNameMap {
    "hometube-activity-dashboard": ActivityDashboard;
  }
}
