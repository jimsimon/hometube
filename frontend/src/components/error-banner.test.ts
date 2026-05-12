/**
 * Tests for `<hometube-error-banner>`.
 *
 * Exercises message rendering, dismissible/retryable variants, and
 * event dispatch.
 */
import { afterEach, describe, expect, it, vi } from "vitest";

import "./error-banner.js";
import type { ErrorBanner } from "./error-banner.js";

afterEach(() => {
  document.body.querySelectorAll("hometube-error-banner").forEach((el) => el.remove());
});

async function mount(attrs: Record<string, string> = {}): Promise<ErrorBanner> {
  const el = document.createElement("hometube-error-banner") as ErrorBanner;
  for (const [key, val] of Object.entries(attrs)) {
    if (val === "") {
      el.toggleAttribute(key, true);
    } else {
      el.setAttribute(key, val);
    }
  }
  document.body.appendChild(el);
  await el.updateComplete;
  return el;
}

describe("<hometube-error-banner>", () => {
  it("renders nothing when message is empty", async () => {
    const el = await mount();
    const banner = el.shadowRoot!.querySelector(".banner");
    expect(banner).toBeNull();
  });

  it("renders the message in a role=alert banner", async () => {
    const el = await mount({ message: "Something went wrong" });
    const banner = el.shadowRoot!.querySelector('[role="alert"]');
    expect(banner).not.toBeNull();
    expect(banner!.textContent).toContain("Something went wrong");
  });

  it("does not show buttons by default", async () => {
    const el = await mount({ message: "Error" });
    const buttons = el.shadowRoot!.querySelectorAll("button");
    expect(buttons.length).toBe(0);
  });

  it("shows Retry button when retryable", async () => {
    const el = await mount({ message: "Error", retryable: "" });
    const btn = el.shadowRoot!.querySelector("button");
    expect(btn).not.toBeNull();
    expect(btn!.textContent).toContain("Retry");
  });

  it("shows Dismiss button when dismissible", async () => {
    const el = await mount({ message: "Error", dismissible: "" });
    const btn = el.shadowRoot!.querySelector('button[aria-label="Dismiss error"]');
    expect(btn).not.toBeNull();
    expect(btn!.textContent).toContain("Dismiss");
  });

  it("dispatches hometube:error-retry on Retry click", async () => {
    const el = await mount({ message: "Error", retryable: "" });
    const handler = vi.fn();
    el.addEventListener("hometube:error-retry", handler);

    const btn = el.shadowRoot!.querySelector("button")!;
    btn.click();

    expect(handler).toHaveBeenCalledOnce();
  });

  it("dispatches hometube:error-dismiss on Dismiss click and clears message", async () => {
    const el = await mount({ message: "Error", dismissible: "" });
    const handler = vi.fn();
    el.addEventListener("hometube:error-dismiss", handler);

    const btn = el.shadowRoot!.querySelector('button[aria-label="Dismiss error"]')!;
    (btn as HTMLElement).click();
    await el.updateComplete;

    expect(handler).toHaveBeenCalledOnce();
    // Message should be cleared, so banner disappears
    const banner = el.shadowRoot!.querySelector(".banner");
    expect(banner).toBeNull();
  });

  it("shows both buttons when both attributes are set", async () => {
    const el = await mount({ message: "Error", retryable: "", dismissible: "" });
    const buttons = el.shadowRoot!.querySelectorAll("button");
    expect(buttons.length).toBe(2);
  });
});
