/**
 * Tests for `<hometube-hidden-undo-toast>`.
 *
 * Verifies the toast surfaces in-page `video-hidden` events, recovers
 * pending hides from sessionStorage (watch-page redirect path), shows
 * inline error feedback when undo fails, and avoids unnecessary
 * reloads for in-page undos.
 */
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import "./hidden-undo-toast.js";
import type { HiddenUndoToast } from "./hidden-undo-toast.js";
import { flushAsync } from "../test-utils.js";

let fetchSpy: ReturnType<typeof vi.fn>;

beforeEach(() => {
  fetchSpy = vi.fn();
  vi.stubGlobal("fetch", fetchSpy);
  sessionStorage.clear();
});

afterEach(() => {
  document.body.querySelectorAll("hometube-hidden-undo-toast").forEach((el) => el.remove());
  vi.restoreAllMocks();
  vi.unstubAllGlobals();
  sessionStorage.clear();
});

function okResponse(status = 204) {
  return Promise.resolve({
    ok: true,
    status,
    headers: new Headers({ "content-type": "application/json" }),
    json: () => Promise.resolve({}),
    text: () => Promise.resolve(""),
  });
}

function errResponse(status: number) {
  return Promise.resolve({
    ok: false,
    status,
    headers: new Headers({ "content-type": "application/json" }),
    json: () => Promise.resolve({ error: "nope" }),
    text: () => Promise.resolve("{}"),
  });
}

async function mount(): Promise<HiddenUndoToast> {
  const el = document.createElement("hometube-hidden-undo-toast") as HiddenUndoToast;
  document.body.appendChild(el);
  await el.updateComplete;
  return el;
}

describe("<hometube-hidden-undo-toast>", () => {
  it("renders nothing initially", async () => {
    const el = await mount();
    expect(el.shadowRoot!.querySelector(".toast")).toBeNull();
  });

  it("appears when a video-hidden event is dispatched", async () => {
    const el = await mount();
    document.dispatchEvent(
      new CustomEvent("video-hidden", { detail: { videoId: "v1", title: "T" } }),
    );
    await el.updateComplete;
    const toast = el.shadowRoot!.querySelector(".toast");
    expect(toast).not.toBeNull();
    expect(toast!.textContent).toContain("T");
  });

  it("undoes an in-page hide without reloading the page", async () => {
    fetchSpy.mockImplementation(() => okResponse());
    const el = await mount();
    const received: Array<{ videoId: string }> = [];
    document.addEventListener("video-unhidden", (e) => {
      received.push((e as CustomEvent).detail);
    });

    document.dispatchEvent(new CustomEvent("video-hidden", { detail: { videoId: "v2" } }));
    await el.updateComplete;
    (el.shadowRoot!.querySelector(".toast button") as HTMLButtonElement).click();
    await flushAsync(el);

    expect(fetchSpy).toHaveBeenCalledTimes(1);
    const [url, init] = fetchSpy.mock.calls[0];
    expect(url).toBe("/api/hidden/v2");
    expect((init as RequestInit).method).toBe("DELETE");
    expect(received).toEqual([{ videoId: "v2" }]);
    // Toast dismisses after a successful in-page undo (no reload).
    expect(el.shadowRoot!.querySelector(".toast")).toBeNull();
  });

  it("consumes a sessionStorage breadcrumb on mount (watch-page redirect path)", async () => {
    sessionStorage.setItem(
      "hometube:pendingHide",
      JSON.stringify({ videoId: "wp1", title: "Watched", at: Date.now() }),
    );
    const el = await mount();
    await el.updateComplete;
    const toast = el.shadowRoot!.querySelector(".toast");
    expect(toast).not.toBeNull();
    expect(toast!.textContent).toContain("Watched");
    // Breadcrumb consumed.
    expect(sessionStorage.getItem("hometube:pendingHide")).toBeNull();
  });

  it("ignores stale breadcrumbs older than the undo window", async () => {
    sessionStorage.setItem(
      "hometube:pendingHide",
      JSON.stringify({ videoId: "old", at: Date.now() - 60_000 }),
    );
    const el = await mount();
    await el.updateComplete;
    expect(el.shadowRoot!.querySelector(".toast")).toBeNull();
  });

  it("surfaces an inline error when the undo DELETE fails", async () => {
    fetchSpy.mockImplementation(() => errResponse(500));
    const el = await mount();
    document.dispatchEvent(new CustomEvent("video-hidden", { detail: { videoId: "fail1" } }));
    await el.updateComplete;
    (el.shadowRoot!.querySelector(".toast button") as HTMLButtonElement).click();
    await flushAsync(el);
    const err = el.shadowRoot!.querySelector(".err");
    expect(err).not.toBeNull();
    expect(err!.textContent).toContain("500");
    // Toast remains visible so the user can read the error.
    expect(el.shadowRoot!.querySelector(".toast")).not.toBeNull();
  });

  it("clears a stale error when a new hide arrives", async () => {
    fetchSpy.mockImplementation(() => errResponse(503));
    const el = await mount();
    document.dispatchEvent(new CustomEvent("video-hidden", { detail: { videoId: "e1" } }));
    await el.updateComplete;
    (el.shadowRoot!.querySelector(".toast button") as HTMLButtonElement).click();
    await flushAsync(el);
    expect(el.shadowRoot!.querySelector(".err")).not.toBeNull();

    document.dispatchEvent(new CustomEvent("video-hidden", { detail: { videoId: "e2" } }));
    await el.updateComplete;
    expect(el.shadowRoot!.querySelector(".err")).toBeNull();
  });
});
