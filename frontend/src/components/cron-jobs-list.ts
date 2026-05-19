/**
 * <hometube-cron-jobs-list>
 *
 * Parent-only list of scheduled cron jobs on the system settings page.
 * Fetches `/api/cron/jobs` on connect and renders one
 * <hometube-cron-job-card> per job. Re-fetches whenever a child card
 * dispatches a bubbling `hometube:cron-changed` event.
 *
 * Renders into light DOM so the existing page-level CSS keeps applying
 * and the cards can talk to each other via DOM events as before.
 */

import { LitElement, html, nothing } from "lit";
import { customElement, state } from "lit/decorators.js";

import { api } from "../services/api.js";
import type { CronJob } from "./cron-job-card.js";
import "./cron-job-card.js";
import "./loading-spinner.js";

@customElement("hometube-cron-jobs-list")
export class CronJobsList extends LitElement {
  // Render in light DOM so page-level styles apply and the
  // <hometube-cron-job-card> children participate in the same event
  // tree as the rest of the page.
  protected createRenderRoot(): this {
    return this;
  }

  @state() private jobs: CronJob[] | null = null;
  @state() private error: string | null = null;

  connectedCallback(): void {
    super.connectedCallback();
    this.addEventListener("hometube:cron-changed", this.handleChanged);
    void this.loadJobs();
  }

  disconnectedCallback(): void {
    this.removeEventListener("hometube:cron-changed", this.handleChanged);
    super.disconnectedCallback();
  }

  private handleChanged = (): void => {
    void this.loadJobs();
  };

  private async loadJobs(): Promise<void> {
    try {
      this.error = null;
      this.jobs = await api.get<CronJob[]>("/api/cron/jobs");
    } catch (err) {
      this.error = err instanceof Error ? err.message : String(err);
    }
  }

  render() {
    if (this.error !== null) {
      return html`<p role="alert">Failed to load jobs: ${this.error}</p>`;
    }
    if (this.jobs === null) {
      return html`<hometube-loading-spinner label="Loading jobs…"></hometube-loading-spinner>`;
    }
    const count = this.jobs.length;
    return html`
      <div role="status" aria-live="polite" class="sr-only">
        Loaded ${count} scheduled job${count === 1 ? "" : "s"}.
      </div>
      ${this.jobs.map(
        (job) => html`
          <hometube-cron-job-card job-id=${String(job.id)} .job=${job}></hometube-cron-job-card>
        `,
      )}
      ${count === 0 ? html`<p>No scheduled jobs.</p>` : nothing}
    `;
  }
}

declare global {
  interface HTMLElementTagNameMap {
    "hometube-cron-jobs-list": CronJobsList;
  }
}
