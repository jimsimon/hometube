/**
 * Tests for `<hometube-video-card>`.
 *
 * Exercises attribute reflection, link generation, duration formatting,
 * progress bar rendering, and conditional thumbnail/channel display.
 */
import { afterEach, describe, expect, it } from "vitest";

import "./video-card.js";
import type { VideoCard } from "./video-card.js";

afterEach(() => {
  document.body.querySelectorAll("hometube-video-card").forEach((el) => el.remove());
});

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
});
