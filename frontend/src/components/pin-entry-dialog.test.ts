/**
 * Tests for `<hometube-pin-entry-dialog>`.
 *
 * Exercises PIN validation, form submission with mocked fetch,
 * error states, and dialog visibility.
 */
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import "./pin-entry-dialog.js";
import type { PinEntryDialog } from "./pin-entry-dialog.js";

let fetchSpy: ReturnType<typeof vi.fn>;

beforeEach(() => {
  fetchSpy = vi.fn();
  vi.stubGlobal("fetch", fetchSpy);
});

afterEach(() => {
  document.body.querySelectorAll("hometube-pin-entry-dialog").forEach((el) => el.remove());
  vi.restoreAllMocks();
  vi.unstubAllGlobals();
});

async function mount(attrs: Record<string, string> = {}): Promise<PinEntryDialog> {
  const el = document.createElement("hometube-pin-entry-dialog") as PinEntryDialog;
  for (const [key, val] of Object.entries(attrs)) {
    el.setAttribute(key, String(val));
  }
  document.body.appendChild(el);
  await el.updateComplete;
  return el;
}

describe("<hometube-pin-entry-dialog>", () => {
  it("renders the wa-dialog with the display name in label", async () => {
    const el = await mount({
      open: "",
      "account-id": "5",
      "display-name": "Parent One",
    });
    const dialog = el.shadowRoot!.querySelector("wa-dialog");
    expect(dialog).not.toBeNull();
    expect(dialog!.getAttribute("label")).toContain("Parent One");
  });

  it("renders a form with PIN input", async () => {
    const el = await mount({ open: "", "account-id": "1" });
    const input = el.shadowRoot!.querySelector('input[name="pin"]') as HTMLInputElement;
    expect(input).not.toBeNull();
    expect(input.getAttribute("type")).toBe("password");
    expect(input.getAttribute("inputmode")).toBe("numeric");
    expect(input.getAttribute("minlength")).toBe("4");
    expect(input.getAttribute("maxlength")).toBe("6");
  });

  it("shows validation error for non-digit PIN", async () => {
    const el = await mount({ open: "", "account-id": "1" });
    const input = el.shadowRoot!.querySelector('input[name="pin"]') as HTMLInputElement;
    const form = el.shadowRoot!.querySelector("form")!;

    input.value = "abc";
    form.dispatchEvent(new Event("submit", { bubbles: true, cancelable: true }));
    await el.updateComplete;

    const error = el.shadowRoot!.querySelector("hometube-error-banner");
    expect(error).not.toBeNull();
  });

  it("shows validation error for too-short PIN", async () => {
    const el = await mount({ open: "", "account-id": "1" });
    const input = el.shadowRoot!.querySelector('input[name="pin"]') as HTMLInputElement;
    const form = el.shadowRoot!.querySelector("form")!;

    input.value = "12";
    form.dispatchEvent(new Event("submit", { bubbles: true, cancelable: true }));
    await el.updateComplete;

    const error = el.shadowRoot!.querySelector("hometube-error-banner");
    expect(error).not.toBeNull();
  });

  it("submits valid PIN and posts to correct endpoint", async () => {
    // Mock successful switch — the component will attempt to set
    // window.location.href but we can't prevent that in browser mode.
    // Instead we make fetch reject slightly to prevent the redirect but
    // still verify the request was made correctly.
    fetchSpy.mockResolvedValue({
      ok: false,
      status: 500,
      headers: new Headers({ "content-type": "application/json" }),
      json: () => Promise.resolve({ error: "test" }),
      text: () => Promise.resolve('{"error":"test"}'),
    });

    const el = await mount({ open: "", "account-id": "7" });
    const input = el.shadowRoot!.querySelector('input[name="pin"]') as HTMLInputElement;
    const form = el.shadowRoot!.querySelector("form")!;

    input.value = "1234";
    form.dispatchEvent(new Event("submit", { bubbles: true, cancelable: true }));
    await new Promise((r) => setTimeout(r, 10));
    await el.updateComplete;

    expect(fetchSpy).toHaveBeenCalled();
    const [url, opts] = fetchSpy.mock.calls[0];
    expect(url).toBe("/api/auth/switch");
    expect(opts.method).toBe("POST");
    const body = JSON.parse(opts.body);
    expect(body.account_id).toBe(7);
    expect(body.pin).toBe("1234");
  });

  it("shows error on 401 response", async () => {
    fetchSpy.mockResolvedValue({
      ok: false,
      status: 401,
      headers: new Headers({ "content-type": "application/json" }),
      json: () => Promise.resolve({ error: "wrong pin" }),
      text: () => Promise.resolve('{"error":"wrong pin"}'),
    });

    const el = await mount({ open: "", "account-id": "1" });
    const input = el.shadowRoot!.querySelector('input[name="pin"]') as HTMLInputElement;
    const form = el.shadowRoot!.querySelector("form")!;

    input.value = "9999";
    form.dispatchEvent(new Event("submit", { bubbles: true, cancelable: true }));
    await new Promise((r) => setTimeout(r, 10));
    await el.updateComplete;

    const error = el.shadowRoot!.querySelector("hometube-error-banner");
    expect(error).not.toBeNull();
  });

  it("dispatches pin-cancelled on cancel click", async () => {
    const el = await mount({ open: "", "account-id": "1", "display-name": "P" });
    const handler = vi.fn();
    el.addEventListener("pin-cancelled", handler);

    const cancelBtn = el.shadowRoot!.querySelector('button[type="button"]') as HTMLButtonElement;
    expect(cancelBtn).not.toBeNull();
    cancelBtn.click();
    await el.updateComplete;

    expect(handler).toHaveBeenCalledOnce();
  });

  it("renders Continue button text", async () => {
    const el = await mount({ open: "", "account-id": "1" });
    const submit = el.shadowRoot!.querySelector('button[type="submit"]');
    expect(submit!.textContent!.trim()).toBe("Continue");
  });
});
