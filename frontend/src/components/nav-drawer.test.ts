/**
 * Tests for `<hometube-nav-drawer>`.
 *
 * Verifies the default link list, attribute propagation, and
 * imperative show/hide/toggle API.
 */
import { afterEach, describe, expect, it } from "vitest";

import "./nav-drawer.js";
import type { NavDrawer } from "./nav-drawer.js";

afterEach(() => {
  document.body.querySelectorAll("hometube-nav-drawer").forEach((el) => el.remove());
});

async function mount(attrs: Record<string, string> = {}): Promise<NavDrawer> {
  const el = document.createElement("hometube-nav-drawer") as NavDrawer;
  for (const [key, val] of Object.entries(attrs)) {
    el.setAttribute(key, String(val));
  }
  document.body.appendChild(el);
  await el.updateComplete;
  return el;
}

describe("<hometube-nav-drawer>", () => {
  it("renders a wa-drawer with default label", async () => {
    const el = await mount();
    const drawer = el.shadowRoot!.querySelector("wa-drawer");
    expect(drawer).not.toBeNull();
    expect(drawer!.getAttribute("label")).toBe("Navigation");
  });

  it("propagates custom label attribute", async () => {
    const el = await mount({ label: "Custom Nav" });
    const drawer = el.shadowRoot!.querySelector("wa-drawer");
    expect(drawer!.getAttribute("label")).toBe("Custom Nav");
  });

  it("propagates placement attribute", async () => {
    const el = await mount({ placement: "end" });
    const drawer = el.shadowRoot!.querySelector("wa-drawer");
    expect(drawer!.getAttribute("placement")).toBe("end");
  });

  it("renders the default child navigation links", async () => {
    const el = await mount();
    const links = el.shadowRoot!.querySelectorAll(".drawer-list a");
    const hrefs = Array.from(links).map((a) => a.getAttribute("href"));
    expect(hrefs).toEqual([
      "/child/home",
      "/child/channels",
      "/child/playlists",
      "/child/bookmarks",
      "/child/downloads",
    ]);
  });

  it("renders link text correctly", async () => {
    const el = await mount();
    const links = el.shadowRoot!.querySelectorAll(".drawer-list a");
    const texts = Array.from(links).map((a) => a.textContent!.trim());
    expect(texts).toEqual(["Home", "Channels", "Playlists", "Bookmarks", "Downloads"]);
  });

  it("exposes show() method", async () => {
    const el = await mount();
    // show() should not throw even if wa-drawer's show isn't wired
    expect(() => el.show()).not.toThrow();
  });

  it("exposes hide() method", async () => {
    const el = await mount();
    expect(() => el.hide()).not.toThrow();
  });

  it("exposes toggle() method", async () => {
    const el = await mount();
    expect(() => el.toggle()).not.toThrow();
  });
});
