/**
 * Tests for `<hometube-change-pin-dialog>`.
 *
 * Covers PIN validation, mismatch detection, request shape sent to
 * `PUT /api/auth/pin`, success state, and cancel-event dispatching.
 */
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import "./change-pin-dialog.js";
import type { ChangePinDialog } from "./change-pin-dialog.js";
import { flushAsync } from "../test-utils.js";

let fetchSpy: ReturnType<typeof vi.fn>;

beforeEach(() => {
  fetchSpy = vi.fn();
  vi.stubGlobal("fetch", fetchSpy);
});

afterEach(() => {
  document.body.querySelectorAll("hometube-change-pin-dialog").forEach((el) => el.remove());
  vi.restoreAllMocks();
  vi.unstubAllGlobals();
});

async function mount(attrs: Record<string, string> = {}): Promise<ChangePinDialog> {
  const el = document.createElement("hometube-change-pin-dialog") as ChangePinDialog;
  // Disable the post-save auto-close timer by default so tests don't
  // leak a wall-clock setTimeout into the teardown phase.
  if (!("auto-close-ms" in attrs)) {
    el.setAttribute("auto-close-ms", "0");
  }
  for (const [key, val] of Object.entries(attrs)) {
    el.setAttribute(key, String(val));
  }
  document.body.appendChild(el);
  await el.updateComplete;
  return el;
}

function setInput(el: ChangePinDialog, name: string, value: string): void {
  const input = el.shadowRoot!.querySelector(`input[name="${name}"]`) as HTMLInputElement;
  input.value = value;
}

describe("<hometube-change-pin-dialog>", () => {
  it("renders a wa-dialog with the 'Change your PIN' label", async () => {
    const el = await mount({ open: "" });
    const dialog = el.shadowRoot!.querySelector("wa-dialog");
    expect(dialog).not.toBeNull();
    expect(dialog!.getAttribute("label")).toBe("Change your PIN");
  });

  it("renders all three PIN inputs", async () => {
    const el = await mount({ open: "" });
    const current = el.shadowRoot!.querySelector('input[name="current-pin"]') as HTMLInputElement;
    const pin = el.shadowRoot!.querySelector('input[name="pin"]') as HTMLInputElement;
    const confirm = el.shadowRoot!.querySelector('input[name="pin-confirm"]') as HTMLInputElement;
    expect(current).not.toBeNull();
    expect(pin).not.toBeNull();
    expect(confirm).not.toBeNull();
    expect(current.getAttribute("autocomplete")).toBe("current-password");
    expect(pin.getAttribute("type")).toBe("password");
    expect(pin.getAttribute("inputmode")).toBe("numeric");
  });

  it("shows validation error for too-short PIN", async () => {
    const el = await mount({ open: "" });
    setInput(el, "current-pin", "1234");
    setInput(el, "pin", "12");
    setInput(el, "pin-confirm", "12");
    el.shadowRoot!.querySelector("form")!.dispatchEvent(
      new Event("submit", { bubbles: true, cancelable: true }),
    );
    await el.updateComplete;
    expect(el.shadowRoot!.querySelector("hometube-error-banner")).not.toBeNull();
    expect(fetchSpy).not.toHaveBeenCalled();
  });

  it("shows error when current PIN is missing", async () => {
    const el = await mount({ open: "" });
    setInput(el, "pin", "1234");
    setInput(el, "pin-confirm", "1234");
    el.shadowRoot!.querySelector("form")!.dispatchEvent(
      new Event("submit", { bubbles: true, cancelable: true }),
    );
    await el.updateComplete;
    expect(el.shadowRoot!.querySelector("hometube-error-banner")).not.toBeNull();
    expect(fetchSpy).not.toHaveBeenCalled();
  });

  it("shows error when new PIN equals current PIN", async () => {
    const el = await mount({ open: "" });
    setInput(el, "current-pin", "1234");
    setInput(el, "pin", "1234");
    setInput(el, "pin-confirm", "1234");
    el.shadowRoot!.querySelector("form")!.dispatchEvent(
      new Event("submit", { bubbles: true, cancelable: true }),
    );
    await el.updateComplete;
    expect(el.shadowRoot!.querySelector("hometube-error-banner")).not.toBeNull();
    expect(fetchSpy).not.toHaveBeenCalled();
  });

  it("shows error when confirmation doesn't match", async () => {
    const el = await mount({ open: "" });
    setInput(el, "current-pin", "1234");
    setInput(el, "pin", "1234");
    setInput(el, "pin-confirm", "5678");
    el.shadowRoot!.querySelector("form")!.dispatchEvent(
      new Event("submit", { bubbles: true, cancelable: true }),
    );
    await el.updateComplete;
    expect(el.shadowRoot!.querySelector("hometube-error-banner")).not.toBeNull();
    expect(fetchSpy).not.toHaveBeenCalled();
  });

  it("PUTs the new PIN with current_pin to /api/auth/pin on valid submit", async () => {
    fetchSpy.mockResolvedValue({
      ok: true,
      status: 204,
      headers: new Headers(),
      json: () => Promise.resolve({}),
      text: () => Promise.resolve(""),
    });

    const el = await mount({ open: "" });
    const saved = vi.fn();
    el.addEventListener("change-pin-saved", saved);

    setInput(el, "current-pin", "1234");
    setInput(el, "pin", "4321");
    setInput(el, "pin-confirm", "4321");
    el.shadowRoot!.querySelector("form")!.dispatchEvent(
      new Event("submit", { bubbles: true, cancelable: true }),
    );
    await flushAsync(el);

    expect(fetchSpy).toHaveBeenCalledOnce();
    const [url, opts] = fetchSpy.mock.calls[0];
    expect(url).toBe("/api/auth/pin");
    expect(opts.method).toBe("PUT");
    expect(JSON.parse(opts.body)).toEqual({ pin: "4321", current_pin: "1234" });
    expect(saved).toHaveBeenCalledOnce();
    expect(el.shadowRoot!.textContent).toContain("PIN updated");
  });

  it("shows a friendly error on 403 (wrong current PIN)", async () => {
    fetchSpy.mockResolvedValue({
      ok: false,
      status: 403,
      headers: new Headers({ "content-type": "application/json" }),
      json: () => Promise.resolve({ error: "forbidden" }),
      text: () => Promise.resolve('{"error":"forbidden"}'),
    });

    const el = await mount({ open: "" });
    setInput(el, "current-pin", "0000");
    setInput(el, "pin", "4321");
    setInput(el, "pin-confirm", "4321");
    el.shadowRoot!.querySelector("form")!.dispatchEvent(
      new Event("submit", { bubbles: true, cancelable: true }),
    );
    await flushAsync(el);

    const banner = el.shadowRoot!.querySelector("hometube-error-banner") as
      | (HTMLElement & { message?: string })
      | null;
    expect(banner).not.toBeNull();
    expect(banner!.message).toContain("Current PIN");
  });

  it("surfaces server error messages", async () => {
    fetchSpy.mockResolvedValue({
      ok: false,
      status: 400,
      headers: new Headers({ "content-type": "text/plain" }),
      json: () => Promise.reject(new Error("not json")),
      text: () => Promise.resolve("PIN must be 4-6 numeric digits"),
    });

    const el = await mount({ open: "" });
    setInput(el, "current-pin", "1234");
    setInput(el, "pin", "5678");
    setInput(el, "pin-confirm", "5678");
    el.shadowRoot!.querySelector("form")!.dispatchEvent(
      new Event("submit", { bubbles: true, cancelable: true }),
    );
    await flushAsync(el);

    expect(el.shadowRoot!.querySelector("hometube-error-banner")).not.toBeNull();
  });

  it("dispatches change-pin-closed when cancel is clicked", async () => {
    const el = await mount({ open: "" });
    const handler = vi.fn();
    el.addEventListener("change-pin-closed", handler);
    const cancel = el.shadowRoot!.querySelector('button[type="button"]') as HTMLButtonElement;
    cancel.click();
    await el.updateComplete;
    expect(handler).toHaveBeenCalledOnce();
    expect(el.open).toBe(false);
  });
});
