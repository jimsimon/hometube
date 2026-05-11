/**
 * <hometube-usage-limit-editor child-id="...">
 *
 * Form for the seven days of the week. Each row carries the daily-cap
 * (in hours) and an allowed time window (HH:MM start/end). Submitting
 * PUTs the entire set in one transaction.
 */

import { LitElement, html, css, nothing } from 'lit';
import { customElement, property, state } from 'lit/decorators.js';

import { api, ApiError } from '../services/api.js';
import type { UsageLimit } from '../types/index.js';

const DAY_LABELS = [
  'Sunday',
  'Monday',
  'Tuesday',
  'Wednesday',
  'Thursday',
  'Friday',
  'Saturday',
];

function defaultLimit(day: number): UsageLimit {
  return {
    day_of_week: day,
    max_hours: 2,
    allowed_start_time: '08:00',
    allowed_end_time: '20:00',
  };
}

@customElement('hometube-usage-limit-editor')
export class UsageLimitEditor extends LitElement {
  @property({ type: Number, attribute: 'child-id' })
  childId: number | null = null;

  @state() private limits: UsageLimit[] = Array.from({ length: 7 }, (_, i) =>
    defaultLimit(i),
  );
  @state() private saving = false;
  @state() private message = '';
  @state() private error = '';

  static styles = css`
    :host {
      display: block;
    }
    table {
      border-collapse: collapse;
      width: 100%;
    }
    th,
    td {
      padding: 0.5rem;
      border-bottom: 1px solid var(--wa-color-surface-border);
      text-align: left;
    }
    input[type='number'],
    input[type='time'] {
      padding: 0.25rem 0.5rem;
      border: 1px solid var(--wa-color-surface-border, #ccc);
      border-radius: 0.375rem;
      background: var(--wa-color-surface-default);
      color: var(--wa-color-text-normal);
      font: inherit;
      width: 6.5rem;
    }
    button {
      padding: 0.5rem 1rem;
      border-radius: 0.375rem;
      border: 1px solid var(--wa-color-surface-border, #ccc);
      background: var(--wa-color-brand-fill, #2563eb);
      color: white;
      font: inherit;
      cursor: pointer;
      margin-top: 1rem;
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
    if (changed.has('childId') && this.childId != null) {
      void this.refresh();
    }
  }

  private async refresh(): Promise<void> {
    if (this.childId == null) return;
    try {
      const rows = await api.get<UsageLimit[]>(
        `/api/children/${this.childId}/usage-limits`,
      );
      const merged: UsageLimit[] = Array.from({ length: 7 }, (_, day) => {
        const row = rows.find((r) => r.day_of_week === day);
        return row ?? defaultLimit(day);
      });
      this.limits = merged;
    } catch (err) {
      this.error = (err as Error).message;
    }
  }

  private updateField(day: number, field: keyof UsageLimit, value: string): void {
    const next = [...this.limits];
    const row = { ...next[day]! };
    if (field === 'max_hours') {
      row.max_hours = Number(value);
    } else if (field === 'allowed_start_time') {
      row.allowed_start_time = value;
    } else if (field === 'allowed_end_time') {
      row.allowed_end_time = value;
    }
    next[day] = row;
    this.limits = next;
  }

  private async onSave(): Promise<void> {
    if (this.childId == null) return;
    this.saving = true;
    this.message = '';
    this.error = '';
    try {
      await api.put(`/api/children/${this.childId}/usage-limits`, this.limits);
      this.message = 'Saved.';
    } catch (err) {
      this.error = err instanceof ApiError ? String(err.body) : (err as Error).message;
    } finally {
      this.saving = false;
    }
  }

  override render() {
    if (this.childId == null) {
      return html`<p class="empty">Pick a child to set time limits.</p>`;
    }
    return html`
      <table aria-label="Daily usage limits">
        <thead>
          <tr>
            <th scope="col">Day</th>
            <th scope="col">Max hours</th>
            <th scope="col">Allowed start</th>
            <th scope="col">Allowed end</th>
          </tr>
        </thead>
        <tbody>
          ${this.limits.map(
            (row, idx) => html`
              <tr>
                <th scope="row">${DAY_LABELS[idx]}</th>
                <td>
                  <label class="sr-only" for="hours-${idx}"
                    >Max hours for ${DAY_LABELS[idx]}</label
                  >
                  <input
                    id="hours-${idx}"
                    type="number"
                    min="0"
                    max="24"
                    step="0.25"
                    .value=${String(row.max_hours)}
                    @input=${(e: Event) =>
                      this.updateField(
                        idx,
                        'max_hours',
                        (e.target as HTMLInputElement).value,
                      )}
                  />
                </td>
                <td>
                  <label class="sr-only" for="start-${idx}"
                    >Start time for ${DAY_LABELS[idx]}</label
                  >
                  <input
                    id="start-${idx}"
                    type="time"
                    .value=${row.allowed_start_time}
                    @input=${(e: Event) =>
                      this.updateField(
                        idx,
                        'allowed_start_time',
                        (e.target as HTMLInputElement).value,
                      )}
                  />
                </td>
                <td>
                  <label class="sr-only" for="end-${idx}"
                    >End time for ${DAY_LABELS[idx]}</label
                  >
                  <input
                    id="end-${idx}"
                    type="time"
                    .value=${row.allowed_end_time}
                    @input=${(e: Event) =>
                      this.updateField(
                        idx,
                        'allowed_end_time',
                        (e.target as HTMLInputElement).value,
                      )}
                  />
                </td>
              </tr>
            `,
          )}
        </tbody>
      </table>
      <button type="button" @click=${() => void this.onSave()} ?disabled=${this.saving}>
        ${this.saving ? 'Saving…' : 'Save'}
      </button>
      ${this.message
        ? html`<p class="ok" role="status">${this.message}</p>`
        : nothing}
      ${this.error ? html`<p class="error" role="alert">${this.error}</p>` : nothing}
    `;
  }
}

declare global {
  interface HTMLElementTagNameMap {
    'hometube-usage-limit-editor': UsageLimitEditor;
  }
}
