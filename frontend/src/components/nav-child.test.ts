/**
 * Tests for `<hometube-nav-child>`.
 *
 * Verifies composition: nav-bar shell, drawer toggle button,
 * search bar, and theme toggle.
 */
import { afterEach, describe, expect, it } from "vitest";

import "./nav-child.js";
import type { NavChild } from "./nav-child.js";

afterEach(() => {
  document.body.querySelectorAll("hometube-nav-child").forEach((el) => el.remove());
});

async function mount(attrs: Record<string, string> = {}): Promise<NavChild> {
  const el = document.createElement("hometube-nav-child") as NavChild;
  for (const [key, val] of Object.entries(attrs)) {
    el.setAttribute(key, String(val));
  }
  document.body.appendChild(el);
  await el.updateComplete;
  return el;
}

describe("<hometube-nav-child>", () => {
  it("renders a nav-bar with the display name", async () => {
    const el = await mount({ "display-name": "ChildUser" });
    const navBar = el.shadowRoot!.querySelector("hometube-nav-bar");
    expect(navBar).not.toBeNull();
    expect(navBar!.getAttribute("display-name")).toBe("ChildUser");
  });

  it("renders a drawer toggle button with aria-label", async () => {
    const el = await mount();
    const btn = el.shadowRoot!.querySelector('button[aria-label="Open navigation menu"]');
    expect(btn).not.toBeNull();
    expect(btn!.textContent!.trim()).toBe("\u2630"); // hamburger ☰
  });

  it("renders the HomeTube brand link to child home", async () => {
    const el = await mount();
    const brand = el.shadowRoot!.querySelector('a[href="/child/home"].brand');
    expect(brand).not.toBeNull();
    expect(brand!.textContent).toBe("HomeTube");
  });

  it("renders a search bar in the primary slot", async () => {
    const el = await mount();
    const search = el.shadowRoot!.querySelector("hometube-search-bar");
    expect(search).not.toBeNull();
  });

  it("renders a theme toggle in the actions slot", async () => {
    const el = await mount();
    expect(el.shadowRoot!.querySelector("hometube-theme-toggle")).not.toBeNull();
  });

  it("renders a nav drawer", async () => {
    const el = await mount();
    expect(el.shadowRoot!.querySelector("hometube-nav-drawer")).not.toBeNull();
  });
});
