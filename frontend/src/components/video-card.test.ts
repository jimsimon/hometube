/**
 * Tests for `<hometube-video-card>`.
 *
 * Exercises attribute reflection, link generation, duration formatting,
 * progress bar rendering, conditional thumbnail/channel display, the
 * kebab "Hide" menu, the hidden-mode "Unhide" action, and error
 * surfacing.
 */
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import "./video-card.js";
import type { VideoCard } from "./video-card.js";
import { flushAsync } from "../test-utils.js";

let fetchSpy: ReturnType<typeof vi.fn>;

beforeEach(() => {
  fetchSpy = vi.fn();
  vi.stubGlobal("fetch", fetchSpy);
});

afterEach(() => {
  document.body.querySelectorAll("hometube-video-card").forEach((el) => el.remove());
  vi.restoreAllMocks();
  vi.unstubAllGlobals();
});

function okResponse(body: unknown = {}, status = 200) {
  return Promise.resolve({
    ok: true,
    status,
    headers: new Headers({ "content-type": "application/json" }),
    json: () => Promise.resolve(body),
    text: () => Promise.resolve(JSON.stringify(body)),
  });
}

function errResponse(status: number, body: unknown = { error: "nope" }) {
  return Promise.resolve({
    ok: false,
    status,
    headers: new Headers({ "content-type": "application/json" }),
    json: () => Promise.resolve(body),
    text: () => Promise.resolve(JSON.stringify(body)),
  });
}

async function mount(attrs: Record<string, string | number> = {}): Promise<VideoCard> {
  const el = document.createElement("hometube-video-card") as VideoCard;
  for (const [key, val] of Object.entries(attrs)) {
    el.setAttribute(key, String(val));
  }
  document.body.appendChild(el);
  await el.updateComplete;
  return el;
}

describe("<hometube-video-card>", () => {
  it("renders a link to the video page", async () => {
    const el = await mount({ "video-id": "abc123", title: "My Video" });
    const anchor = el.shadowRoot!.querySelector("a");
    expect(anchor).not.toBeNull();
    expect(anchor!.getAttribute("href")).toBe("/child/video/abc123");
    expect(anchor!.getAttribute("aria-label")).toBe("My Video");
  });

  it("renders # href when video-id is empty", async () => {
    const el = await mount({ title: "No ID" });
    const anchor = el.shadowRoot!.querySelector("a");
    expect(anchor!.getAttribute("href")).toBe("#");
  });

  it("renders the thumbnail when url is provided", async () => {
    const el = await mount({
      "video-id": "v1",
      title: "T",
      "thumbnail-url": "https://img.test/thumb.jpg",
    });
    const img = el.shadowRoot!.querySelector("img");
    expect(img).not.toBeNull();
    expect(img!.getAttribute("src")).toBe("https://img.test/thumb.jpg");
    expect(img!.getAttribute("loading")).toBe("lazy");
  });

  it("does not render an img when no thumbnail-url", async () => {
    const el = await mount({ "video-id": "v2", title: "No Thumb" });
    const img = el.shadowRoot!.querySelector("img");
    expect(img).toBeNull();
  });

  it("renders the channel title", async () => {
    const el = await mount({
      "video-id": "v3",
      title: "Vid",
      "channel-title": "Cool Channel",
    });
    const channel = el.shadowRoot!.querySelector(".channel");
    expect(channel).not.toBeNull();
    expect(channel!.textContent).toBe("Cool Channel");
  });

  it("does not render channel div when absent", async () => {
    const el = await mount({ "video-id": "v4", title: "Solo" });
    const channel = el.shadowRoot!.querySelector(".channel");
    expect(channel).toBeNull();
  });

  it("formats short duration as M:SS", async () => {
    const el = await mount({ "video-id": "v5", title: "D", duration: "125" });
    const badge = el.shadowRoot!.querySelector(".duration");
    expect(badge).not.toBeNull();
    expect(badge!.textContent).toBe("2:05");
  });

  it("formats long duration as H:MM:SS", async () => {
    const el = await mount({ "video-id": "v6", title: "Long", duration: "3661" });
    const badge = el.shadowRoot!.querySelector(".duration");
    expect(badge!.textContent).toBe("1:01:01");
  });

  it("does not render duration badge when absent", async () => {
    const el = await mount({ "video-id": "v7", title: "NoDur" });
    const badge = el.shadowRoot!.querySelector(".duration");
    expect(badge).toBeNull();
  });

  it("renders progress bar when progress > 0", async () => {
    const el = await mount({ "video-id": "v8", title: "P", progress: "0.5" });
    const bar = el.shadowRoot!.querySelector(".progress span") as HTMLElement;
    expect(bar).not.toBeNull();
    expect(bar.style.width).toBe("50%");
  });

  it("does not render progress bar when progress is 0", async () => {
    const el = await mount({ "video-id": "v9", title: "NoProg" });
    const bar = el.shadowRoot!.querySelector(".progress");
    expect(bar).toBeNull();
  });

  it("clamps progress to 0-100%", async () => {
    const el = await mount({ "video-id": "v10", title: "Over", progress: "1.5" });
    const bar = el.shadowRoot!.querySelector(".progress span") as HTMLElement;
    expect(bar.style.width).toBe("100%");
  });

  it("renders the title text", async () => {
    const el = await mount({ "video-id": "v11", title: "Hello World" });
    const titleDiv = el.shadowRoot!.querySelector(".title");
    expect(titleDiv!.textContent).toBe("Hello World");
  });

  describe("kebab menu + Hide action", () => {
    it("toggles the menu open and closed when the kebab is clicked", async () => {
      const el = await mount({ "video-id": "k1", title: "K" });
      const kebab = el.shadowRoot!.querySelector(".kebab") as HTMLButtonElement;
      expect(el.shadowRoot!.querySelector(".menu")).toBeNull();
      kebab.click();
      await el.updateComplete;
      expect(el.shadowRoot!.querySelector(".menu")).not.toBeNull();
      kebab.click();
      await el.updateComplete;
      expect(el.shadowRoot!.querySelector(".menu")).toBeNull();
    });

    it("closes the menu when Escape is pressed", async () => {
      const el = await mount({ "video-id": "k2", title: "K2" });
      (el.shadowRoot!.querySelector(".kebab") as HTMLButtonElement).click();
      await el.updateComplete;
      expect(el.shadowRoot!.querySelector(".menu")).not.toBeNull();
      document.dispatchEvent(new KeyboardEvent("keydown", { key: "Escape" }));
      await el.updateComplete;
      expect(el.shadowRoot!.querySelector(".menu")).toBeNull();
    });

    it("closes the menu on outside click", async () => {
      const el = await mount({ "video-id": "k3", title: "K3" });
      (el.shadowRoot!.querySelector(".kebab") as HTMLButtonElement).click();
      await el.updateComplete;
      expect(el.shadowRoot!.querySelector(".menu")).not.toBeNull();
      document.body.click();
      await el.updateComplete;
      expect(el.shadowRoot!.querySelector(".menu")).toBeNull();
    });

    it("POSTs /api/hidden and dispatches video-hidden on Hide", async () => {
      fetchSpy.mockImplementation(() => okResponse({ id: 1, video_id: "h1" }));
      const el = await mount({
        "video-id": "h1",
        title: "Hide Me",
        "channel-id": "ch1",
        "channel-title": "ChName",
        "thumbnail-url": "https://t.test/x.jpg",
      });
      const received: Array<{ videoId: string }> = [];
      el.addEventListener("video-hidden", (e) => {
        received.push((e as CustomEvent).detail);
      });
      (el.shadowRoot!.querySelector(".kebab") as HTMLButtonElement).click();
      await el.updateComplete;
      const hideBtn = el.shadowRoot!.querySelector(".menu button") as HTMLButtonElement;
      hideBtn.click();
      await flushAsync(el);

      expect(fetchSpy).toHaveBeenCalledTimes(1);
      const [url, init] = fetchSpy.mock.calls[0];
      expect(url).toBe("/api/hidden");
      expect((init as RequestInit).method).toBe("POST");
      const body = JSON.parse((init as RequestInit).body as string);
      expect(body.video_id).toBe("h1");
      expect(body.channel_id).toBe("ch1");
      expect(received).toEqual([{ videoId: "h1", title: "Hide Me" }]);
      // Menu closes after a successful hide.
      expect(el.shadowRoot!.querySelector(".menu")).toBeNull();
    });

    it("surfaces an error message when the Hide POST fails", async () => {
      fetchSpy.mockImplementation(() => errResponse(503));
      const el = await mount({ "video-id": "h2", title: "Err" });
      (el.shadowRoot!.querySelector(".kebab") as HTMLButtonElement).click();
      await el.updateComplete;
      (el.shadowRoot!.querySelector(".menu button") as HTMLButtonElement).click();
      await flushAsync(el);
      const err = el.shadowRoot!.querySelector(".action-error");
      expect(err).not.toBeNull();
      expect(err!.textContent).toContain("503");
    });

    it("does nothing when Hide is clicked without a video-id", async () => {
      const el = await mount({ title: "no id" });
      (el.shadowRoot!.querySelector(".kebab") as HTMLButtonElement).click();
      await el.updateComplete;
      (el.shadowRoot!.querySelector(".menu button") as HTMLButtonElement).click();
      await flushAsync(el);
      expect(fetchSpy).not.toHaveBeenCalled();
    });
  });

  describe('mode="hidden" (Unhide)', () => {
    it("renders an Unhide button and no kebab", async () => {
      const el = await mount({ "video-id": "u1", title: "U", mode: "hidden" });
      expect(el.shadowRoot!.querySelector(".kebab")).toBeNull();
      const btn = el.shadowRoot!.querySelector(".unhide-btn") as HTMLButtonElement;
      expect(btn).not.toBeNull();
      expect(btn.textContent!.trim()).toBe("Unhide");
    });

    it("DELETEs /api/hidden/:id and dispatches video-unhidden", async () => {
      fetchSpy.mockImplementation(() => okResponse({}, 204));
      const el = await mount({ "video-id": "u2", title: "U2", mode: "hidden" });
      const received: Array<{ videoId: string }> = [];
      el.addEventListener("video-unhidden", (e) => {
        received.push((e as CustomEvent).detail);
      });
      (el.shadowRoot!.querySelector(".unhide-btn") as HTMLButtonElement).click();
      await flushAsync(el);
      expect(fetchSpy).toHaveBeenCalledTimes(1);
      const [url, init] = fetchSpy.mock.calls[0];
      expect(url).toBe("/api/hidden/u2");
      expect((init as RequestInit).method).toBe("DELETE");
      expect(received).toEqual([{ videoId: "u2" }]);
    });

    it("surfaces an error when Unhide DELETE fails", async () => {
      fetchSpy.mockImplementation(() => errResponse(500));
      const el = await mount({ "video-id": "u3", title: "U3", mode: "hidden" });
      (el.shadowRoot!.querySelector(".unhide-btn") as HTMLButtonElement).click();
      await flushAsync(el);
      const err = el.shadowRoot!.querySelector(".action-error");
      expect(err).not.toBeNull();
      expect(err!.textContent).toContain("500");
    });
  });
});
