/**
 * <hometube-cron-job-card>
 *
 * Shows a single cron job with:
 *   - name + description
 *   - current preset (dropdown of `allowed_presets`)
 *   - enabled toggle
 *   - last-run badge (success/failure + timestamp)
 *   - next-run timestamp
 *   - "Run Now" button
 *   - expandable run history
 *
 * Dispatches a bubbling `hometube:cron-changed` event whenever a
 * mutation succeeds so the parent page re-fetches the full list.
 *
 * The "Run Now" + status updates announce themselves via an internal
 * ARIA live region.
 */

import { LitElement, html, css, nothing } from 'lit';
import { customElement, property, state } from 'lit/decorators.js';

import { api } from '../services/api.js';

interface CronJob {
  id: number;
  name: string;
  description: string | null;
  job_type: string;
  schedule: string;
  schedule_preset: string;
  allowed_presets: string[];
  enabled: boolean;
  last_run_at: number | null;
  last_run_status: string | null;
  last_run_message: string | null;
  next_run_at: number | null;
}

interface CronRun {
  id: number;
  job_id: number;
  started_at: number;
  finished_at: number | null;
  status: 'running' | 'success' | 'failure';
  message: string | null;
  output: string | null;
}

@customElement('hometube-cron-job-card')
export class CronJobCard extends LitElement {
  @property({ type: Number, attribute: 'job-id' }) jobId = 0;
  @property({ attribute: false }) job: CronJob | null = null;

  @state() private runs: CronRun[] = [];
  @state() private historyOpen = false;
  @state() private busy = false;
  @state() private status = '';

  static styles = css`
    :host {
      display: block;
      margin-bottom: 1rem;
    }
    article {
      padding: 1rem;
      border: 1px solid var(--wa-color-surface-border);
      border-radius: 0.5rem;
      background: var(--wa-color-surface-default);
    }
    header {
      display: flex;
      gap: 1rem;
      align-items: center;
      flex-wrap: wrap;
      justify-content: space-between;
    }
    h3 {
      margin: 0;
      font-size: 1.05rem;
    }
    .description {
      color: var(--wa-color-text-quiet);
      margin: 0.25rem 0 0;
    }
    .controls {
      display: flex;
      gap: 0.75rem;
      align-items: center;
      flex-wrap: wrap;
      margin-top: 0.75rem;
    }
    select,
    button {
      padding: 0.4rem 0.75rem;
      border-radius: 0.375rem;
      border: 1px solid var(--wa-color-surface-border, #ccc);
      background: var(--wa-color-surface-default);
      color: var(--wa-color-text-normal);
      font: inherit;
    }
    button {
      cursor: pointer;
    }
    button.primary {
      background: var(--wa-color-brand-fill, #2563eb);
      color: white;
      border-color: transparent;
    }
    .badge {
      display: inline-flex;
      align-items: center;
      gap: 0.25rem;
      font-size: 0.85rem;
      padding: 0.15rem 0.5rem;
      border-radius: 999px;
      background: var(--wa-color-surface-raised);
    }
    .badge.success {
      background: var(--wa-color-success-quiet, rgba(34, 197, 94, 0.15));
      color: var(--wa-color-success-on-quiet, #166534);
    }
    .badge.failure {
      background: var(--wa-color-danger-quiet, rgba(185, 28, 28, 0.15));
      color: var(--wa-color-danger-on-quiet, #991b1b);
    }
    .runs {
      margin-top: 0.75rem;
      padding-left: 1rem;
    }
    .runs li {
      list-style: disc;
      margin-bottom: 0.25rem;
    }
    .live {
      position: absolute;
      width: 1px;
      height: 1px;
      overflow: hidden;
      clip: rect(0 0 0 0);
      white-space: nowrap;
    }
    pre {
      max-height: 12rem;
      overflow: auto;
      padding: 0.5rem;
      background: var(--wa-color-surface-raised);
      border-radius: 0.375rem;
      font-size: 0.8rem;
    }
  `;

  private fmtDate(ts: number | null): string {
    if (!ts) return '—';
    return new Date(ts * 1000).toLocaleString();
  }

  private async onTogglePreset(e: Event): Promise<void> {
    if (!this.job) return;
    const preset = (e.target as HTMLSelectElement).value;
    this.busy = true;
    try {
      this.job = await api.put<CronJob>(`/api/cron/jobs/${this.jobId}`, {
        schedule_preset: preset,
      });
      this.status = `Schedule updated to "${preset}".`;
      this.dispatchChanged();
    } catch (err) {
      this.status = `Failed to update: ${(err as Error).message}`;
    } finally {
      this.busy = false;
    }
  }

  private async onToggleEnabled(e: Event): Promise<void> {
    if (!this.job) return;
    const enabled = (e.target as HTMLInputElement).checked;
    this.busy = true;
    try {
      this.job = await api.put<CronJob>(`/api/cron/jobs/${this.jobId}`, {
        enabled,
      });
      this.status = enabled ? 'Job enabled.' : 'Job disabled.';
      this.dispatchChanged();
    } catch (err) {
      this.status = `Failed to toggle: ${(err as Error).message}`;
    } finally {
      this.busy = false;
    }
  }

  private async onRunNow(): Promise<void> {
    this.busy = true;
    this.status = 'Running…';
    try {
      await api.post(`/api/cron/jobs/${this.jobId}/run`);
      this.status = 'Triggered. Refreshing in a moment…';
      // After a couple of seconds reload the parent list to surface
      // the result.
      setTimeout(() => this.dispatchChanged(), 2000);
    } catch (err) {
      this.status = `Run failed: ${(err as Error).message}`;
    } finally {
      this.busy = false;
    }
  }

  private async toggleHistory(): Promise<void> {
    this.historyOpen = !this.historyOpen;
    if (this.historyOpen && this.runs.length === 0) {
      try {
        this.runs = await api.get<CronRun[]>(
          `/api/cron/jobs/${this.jobId}/runs?limit=20`,
        );
      } catch (err) {
        this.status = `Could not load history: ${(err as Error).message}`;
      }
    }
  }

  private dispatchChanged(): void {
    document.dispatchEvent(
      new CustomEvent('hometube:cron-changed', {
        bubbles: true,
        composed: true,
      }),
    );
  }

  override render() {
    if (!this.job) return html`<p>Loading job…</p>`;
    const status = this.job.last_run_status;
    const badgeClass = status ? `badge ${status}` : 'badge';
    return html`
      <article aria-labelledby="job-${this.jobId}-name">
        <header>
          <div>
            <h3 id="job-${this.jobId}-name">${this.job.name}</h3>
            ${this.job.description
              ? html`<p class="description">${this.job.description}</p>`
              : nothing}
          </div>
          <div>
            <span class=${badgeClass}>
              ${status ?? 'never run'} ·
              ${this.fmtDate(this.job.last_run_at)}
            </span>
          </div>
        </header>
        <div class="controls">
          <label>
            Frequency
            <select
              ?disabled=${this.busy}
              .value=${this.job.schedule_preset}
              @change=${this.onTogglePreset}
            >
              ${this.job.allowed_presets.map(
                (p) => html`<option
                  value=${p}
                  ?selected=${p === this.job!.schedule_preset}
                >
                  ${p}
                </option>`,
              )}
              ${this.job.allowed_presets.includes(this.job.schedule_preset)
                ? nothing
                : html`<option
                    value=${this.job.schedule_preset}
                    selected
                  >
                    ${this.job.schedule_preset} (custom)
                  </option>`}
            </select>
          </label>
          <label>
            <input
              type="checkbox"
              ?checked=${this.job.enabled}
              ?disabled=${this.busy}
              @change=${this.onToggleEnabled}
            />
            Enabled
          </label>
          <span>Next: ${this.fmtDate(this.job.next_run_at)}</span>
          <button
            type="button"
            class="primary"
            ?disabled=${this.busy}
            @click=${this.onRunNow}
          >
            Run now
          </button>
          <button type="button" @click=${this.toggleHistory}>
            ${this.historyOpen ? 'Hide history' : 'Show history'}
          </button>
        </div>
        <div class="live" role="status" aria-live="polite">${this.status}</div>
        ${this.historyOpen
          ? html`<ul class="runs">
              ${this.runs.length === 0
                ? html`<li>No runs yet.</li>`
                : this.runs.map(
                    (r) => html`<li>
                      <strong>${r.status}</strong> ·
                      ${this.fmtDate(r.started_at)}
                      ${r.message ? html` — ${r.message}` : nothing}
                      ${r.output
                        ? html`<details>
                            <summary>Output</summary>
                            <pre>${r.output}</pre>
                          </details>`
                        : nothing}
                    </li>`,
                  )}
            </ul>`
          : nothing}
      </article>
    `;
  }
}

declare global {
  interface HTMLElementTagNameMap {
    'hometube-cron-job-card': CronJobCard;
  }
}
