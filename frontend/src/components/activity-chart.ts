/**
 * <hometube-activity-chart data="[{date,minutes}]">
 *
 * Tiny canvas-based bar chart for daily watch totals. No chart library
 * — we render directly to a `<canvas>` and re-paint on `data` change
 * or when the host element is resized.
 *
 * Theme-aware: bar / axis / text colours are read from CSS custom
 * properties (`--wa-color-brand-fill`, `--wa-color-text-quiet`, etc.)
 * via getComputedStyle on every paint, so a theme switch picks up the
 * new colours next time the user visits the page.
 *
 * A11y: the canvas has an `aria-label` summarising "30 days of watch
 * activity, peaking at … minutes on …", and the component renders a
 * visually-hidden `<table>` mirror so screen readers can read the
 * underlying data row by row.
 */

import { LitElement, html, css } from "lit";
import { customElement, property, query, state } from "lit/decorators.js";

interface DailyEntry {
  date: string;
  minutes: number;
}

@customElement("hometube-activity-chart")
export class ActivityChart extends LitElement {
  /**
   * JSON string `[{date:"YYYY-MM-DD", minutes:N}]`. Setting this as an
   * attribute lets server-rendered HTML pre-populate the chart.
   */
  @property({ type: String })
  data = "[]";

  @state() private parsed: DailyEntry[] = [];

  @query("canvas") private canvas!: HTMLCanvasElement;

  static styles = css`
    :host {
      display: block;
      width: 100%;
    }
    .wrap {
      position: relative;
      width: 100%;
      height: 14rem;
    }
    canvas {
      width: 100%;
      height: 100%;
      display: block;
    }
    .sr-only {
      position: absolute;
      clip: rect(1px, 1px, 1px, 1px);
      width: 1px;
      height: 1px;
      overflow: hidden;
    }
  `;

  override updated(changed: Map<string, unknown>): void {
    if (changed.has("data")) {
      this.parsed = this.parseData();
    }
    this.draw();
  }

  override connectedCallback(): void {
    super.connectedCallback();
    this.parsed = this.parseData();
    window.addEventListener("resize", this.onResize);
  }

  override disconnectedCallback(): void {
    super.disconnectedCallback();
    window.removeEventListener("resize", this.onResize);
  }

  private onResize = (): void => {
    this.draw();
  };

  private parseData(): DailyEntry[] {
    try {
      const v = JSON.parse(this.data);
      if (!Array.isArray(v)) return [];
      return v.filter(
        (x): x is DailyEntry => typeof x?.date === "string" && typeof x?.minutes === "number",
      );
    } catch {
      return [];
    }
  }

  private draw(): void {
    const canvas = this.canvas;
    if (!canvas) return;
    const ctx = canvas.getContext("2d");
    if (!ctx) return;

    const dpr = window.devicePixelRatio || 1;
    const rect = canvas.getBoundingClientRect();
    canvas.width = Math.max(1, Math.floor(rect.width * dpr));
    canvas.height = Math.max(1, Math.floor(rect.height * dpr));
    ctx.setTransform(1, 0, 0, 1, 0, 0);
    ctx.scale(dpr, dpr);
    ctx.clearRect(0, 0, rect.width, rect.height);

    const styles = getComputedStyle(this);
    const barColor = styles.getPropertyValue("--wa-color-brand-fill").trim() || "#2563eb";
    const textColor = styles.getPropertyValue("--wa-color-text-quiet").trim() || "#6b7280";
    const axisColor = styles.getPropertyValue("--wa-color-surface-border").trim() || "#d1d5db";

    if (this.parsed.length === 0) {
      ctx.fillStyle = textColor;
      ctx.font = "12px system-ui, sans-serif";
      ctx.fillText("No activity yet.", 8, 20);
      return;
    }

    const padLeft = 36;
    const padRight = 8;
    const padTop = 8;
    const padBottom = 22;
    const w = rect.width - padLeft - padRight;
    const h = rect.height - padTop - padBottom;

    const maxMinutes = Math.max(60, ...this.parsed.map((d) => d.minutes));
    const barWidth = w / this.parsed.length;

    // Axis line.
    ctx.strokeStyle = axisColor;
    ctx.beginPath();
    ctx.moveTo(padLeft, padTop + h);
    ctx.lineTo(padLeft + w, padTop + h);
    ctx.stroke();

    // Y-axis labels (0, max/2, max).
    ctx.fillStyle = textColor;
    ctx.font = "11px system-ui, sans-serif";
    ctx.textBaseline = "middle";
    ctx.textAlign = "right";
    const yTicks = [0, Math.round(maxMinutes / 2), maxMinutes];
    for (const t of yTicks) {
      const y = padTop + h - (t / maxMinutes) * h;
      ctx.fillText(`${t}m`, padLeft - 4, y);
      ctx.strokeStyle = axisColor;
      ctx.globalAlpha = 0.3;
      ctx.beginPath();
      ctx.moveTo(padLeft, y);
      ctx.lineTo(padLeft + w, y);
      ctx.stroke();
      ctx.globalAlpha = 1;
    }

    // Bars.
    ctx.fillStyle = barColor;
    for (let i = 0; i < this.parsed.length; i++) {
      const d = this.parsed[i];
      if (!d) continue;
      const barH = (d.minutes / maxMinutes) * h;
      const x = padLeft + i * barWidth + 1;
      const y = padTop + h - barH;
      ctx.fillRect(x, y, Math.max(1, barWidth - 2), barH);
    }

    // X-axis labels: first / middle / last.
    ctx.fillStyle = textColor;
    ctx.textAlign = "center";
    const labels: { i: number; date: string }[] = [];
    if (this.parsed.length > 0) {
      const first = this.parsed[0];
      const last = this.parsed[this.parsed.length - 1];
      const middleIdx = Math.floor(this.parsed.length / 2);
      const middle = this.parsed[middleIdx];
      if (first) labels.push({ i: 0, date: first.date });
      if (middle && middleIdx !== 0 && middleIdx !== this.parsed.length - 1) {
        labels.push({ i: middleIdx, date: middle.date });
      }
      if (last && this.parsed.length > 1) {
        labels.push({ i: this.parsed.length - 1, date: last.date });
      }
    }
    for (const lab of labels) {
      const x = padLeft + lab.i * barWidth + barWidth / 2;
      ctx.fillText(this.shortDate(lab.date), x, padTop + h + 12);
    }
  }

  private shortDate(iso: string): string {
    // "YYYY-MM-DD" → "Mar 14"
    const parts = iso.split("-");
    if (parts.length !== 3) return iso;
    const month = Number(parts[1]);
    const day = Number(parts[2]);
    const months = [
      "Jan",
      "Feb",
      "Mar",
      "Apr",
      "May",
      "Jun",
      "Jul",
      "Aug",
      "Sep",
      "Oct",
      "Nov",
      "Dec",
    ];
    if (Number.isNaN(month) || Number.isNaN(day) || month < 1 || month > 12) {
      return iso;
    }
    return `${months[month - 1]} ${day}`;
  }

  private summary(): string {
    if (this.parsed.length === 0) return "No activity yet.";
    let total = 0;
    let peakDay = this.parsed[0]!;
    for (const d of this.parsed) {
      total += d.minutes;
      if (d.minutes > peakDay.minutes) peakDay = d;
    }
    return `${this.parsed.length} days of watch activity, ${total} minutes total, peak ${peakDay.minutes} minutes on ${peakDay.date}.`;
  }

  override render() {
    const summary = this.summary();
    return html`
      <div class="wrap">
        <canvas role="img" aria-label=${summary}></canvas>
      </div>
      <table class="sr-only" aria-label="Daily watch activity">
        <thead>
          <tr>
            <th scope="col">Date</th>
            <th scope="col">Minutes</th>
          </tr>
        </thead>
        <tbody>
          ${this.parsed.map(
            (d) => html`
              <tr>
                <td>${d.date}</td>
                <td>${d.minutes}</td>
              </tr>
            `,
          )}
        </tbody>
      </table>
    `;
  }
}

declare global {
  interface HTMLElementTagNameMap {
    "hometube-activity-chart": ActivityChart;
  }
}
