/**
 * Tests for `<hometube-channel-card>`.
 *
 * Focus: the avatar `<img src>` is routed through the channel-thumbnail
 * cache proxy (`/api/proxy/channel-thumbnail/:id`) when the channel both
 * has an avatar and a known ID, and falls back to a placeholder
 * otherwise.
 */
import { afterEach, describe, expect, it } from "vitest";

import "./channel-card.js";
import type { ChannelCard } from "./channel-card.js";

afterEach(() => {
  document.body.querySelectorAll("hometube-channel-card").forEach((el) => el.remove());
});

async function mount(props: Partial<ChannelCard> = {}): Promise<ChannelCard> {
  const el = document.createElement("hometube-channel-card") as ChannelCard;
  Object.assign(el, props);
  document.body.appendChild(el);
  await el.updateComplete;
  return el;
}

describe("<hometube-channel-card>", () => {
  it("routes the avatar through the cache proxy keyed by channel id", async () => {
    const el = await mount({
      channelId: "UC123",
      title: "Cool Channel",
      thumbnailUrl: "https://yt3.ggpht.com/avatar.jpg",
    });
    const img = el.shadowRoot!.querySelector("img");
    expect(img).not.toBeNull();
    expect(img!.getAttribute("src")).toBe("/api/proxy/channel-thumbnail/UC123");
  });

  it("encodes the channel id in the proxy URL", async () => {
    const el = await mount({
      channelId: "UC a/b",
      title: "Weird",
      thumbnailUrl: "https://yt3.ggpht.com/avatar.jpg",
    });
    const img = el.shadowRoot!.querySelector("img");
    expect(img!.getAttribute("src")).toBe("/api/proxy/channel-thumbnail/UC%20a%2Fb");
  });

  it("shows a placeholder (no img) when the channel has no avatar", async () => {
    const el = await mount({ channelId: "UC123", title: "No Avatar", thumbnailUrl: null });
    expect(el.shadowRoot!.querySelector("img")).toBeNull();
    expect(el.shadowRoot!.querySelector(".placeholder")).not.toBeNull();
  });

  it("falls back to the direct URL when there is no channel id", async () => {
    const el = await mount({
      channelId: "",
      title: "Direct",
      thumbnailUrl: "https://yt3.ggpht.com/avatar.jpg",
    });
    const img = el.shadowRoot!.querySelector("img");
    expect(img!.getAttribute("src")).toBe("https://yt3.ggpht.com/avatar.jpg");
  });
});
