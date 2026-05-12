/**
 * Tests for `<hometube-usage-limit-overlay>`.
 *
 * Exercises the event-driven show/hide lifecycle, the two reason
 * variants, and the time formatting.
 */
import { afterEach, describe, expect, it } from "vitest";

import "./usage-limit-overlay.js";
import type { UsageLimitOverlay } from "./usage-limit-overlay.js";

afterEach(() => {
  document.body.querySelectorAll("hometube-usage-limit-overlay").forEach((el) => el.remove());
});

async function mount(): Promise<UsageLimitOverlay> {
  const el = document.createElement("hometube-usage-limit-overlay") as UsageLimitOverlay;
  document.body.appendChild(el);
  await el.updateComplete;
  return el;
}

function fireUsageLimit(
  reason: "limit_exceeded" | "outside_window",
  allowedWindow?: { start: string; end: string } | null,
): void {
  document.dispatchEvent(
    new CustomEvent("hometube:usage-limit", {
      detail: { reason, allowed_window: allowedWindow ?? null },
    }),
  );
}

describe("<hometube-usage-limit-overlay>", () => {
  it("renders nothing initially (not open)", async () => {
    const el = await mount();
    const backdrop = el.shadowRoot!.querySelector(".backdrop");
    expect(backdrop).toBeNull();
  });

  it("shows the overlay on limit_exceeded event", async () => {
    const el = await mount();
    fireUsageLimit("limit_exceeded");
    await el.updateComplete;

    const backdrop = el.shadowRoot!.querySelector(".backdrop");
    expect(backdrop).not.toBeNull();
    const heading = el.shadowRoot!.querySelector("h2");
    expect(heading!.textContent).toBe("All done for today!");
    const body = el.shadowRoot!.querySelector("p");
    expect(body!.textContent).toContain("used up your time");
  });

  it("shows the overlay on outside_window event with time", async () => {
    const el = await mount();
    fireUsageLimit("outside_window", { start: "08:00", end: "20:00" });
    await el.updateComplete;

    const heading = el.shadowRoot!.querySelector("h2");
    expect(heading!.textContent).toContain("outside your viewing hours");
    const body = el.shadowRoot!.querySelector("p");
    expect(body!.textContent).toContain("8:00 AM");
  });

  it("shows generic message for outside_window without start time", async () => {
    const el = await mount();
    fireUsageLimit("outside_window", null);
    await el.updateComplete;

    const body = el.shadowRoot!.querySelector("p");
    expect(body!.textContent).toContain("Come back during your allowed hours");
  });

  it("closes on OK button click", async () => {
    const el = await mount();
    fireUsageLimit("limit_exceeded");
    await el.updateComplete;

    const btn = el.shadowRoot!.querySelector("button");
    expect(btn).not.toBeNull();
    btn!.click();
    await el.updateComplete;

    const backdrop = el.shadowRoot!.querySelector(".backdrop");
    expect(backdrop).toBeNull();
  });

  it("has proper ARIA attributes for the dialog", async () => {
    const el = await mount();
    fireUsageLimit("limit_exceeded");
    await el.updateComplete;

    const dialog = el.shadowRoot!.querySelector('[role="dialog"]');
    expect(dialog).not.toBeNull();
    expect(dialog!.getAttribute("aria-modal")).toBe("true");
    expect(dialog!.getAttribute("aria-labelledby")).toBe("usage-overlay-title");
  });

  it("formats PM times correctly", async () => {
    const el = await mount();
    fireUsageLimit("outside_window", { start: "14:30", end: "20:00" });
    await el.updateComplete;

    const body = el.shadowRoot!.querySelector("p");
    expect(body!.textContent).toContain("2:30 PM");
  });

  it("formats noon correctly", async () => {
    const el = await mount();
    fireUsageLimit("outside_window", { start: "12:00", end: "20:00" });
    await el.updateComplete;

    const body = el.shadowRoot!.querySelector("p");
    expect(body!.textContent).toContain("12:00 PM");
  });

  it("formats midnight correctly", async () => {
    const el = await mount();
    fireUsageLimit("outside_window", { start: "00:00", end: "20:00" });
    await el.updateComplete;

    const body = el.shadowRoot!.querySelector("p");
    expect(body!.textContent).toContain("12:00 AM");
  });
});
