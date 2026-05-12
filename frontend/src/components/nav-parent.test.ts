/**
 * Tests for `<hometube-nav-parent>`.
 *
 * Verifies composition of child components, navigation links,
 * display-name propagation, and event re-dispatch.
 */
import { afterEach, describe, expect, it, vi } from "vitest";

import "./nav-parent.js";
import type { NavParent } from "./nav-parent.js";

afterEach(() => {
  document.body.querySelectorAll("hometube-nav-parent").forEach((el) => el.remove());
});

async function mount(attrs: Record<string, string> = {}): Promise<NavParent> {
  const el = document.createElement("hometube-nav-parent") as NavParent;
  for (const [key, val] of Object.entries(attrs)) {
    el.setAttribute(key, String(val));
  }
  document.body.appendChild(el);
  await el.updateComplete;
  return el;
}

describe("<hometube-nav-parent>", () => {
  it("renders a nav-bar with the display name", async () => {
    const el = await mount({ "display-name": "ParentUser" });
    const navBar = el.shadowRoot!.querySelector("hometube-nav-bar");
    expect(navBar).not.toBeNull();
    expect(navBar!.getAttribute("display-name")).toBe("ParentUser");
  });

  it("renders parent navigation links", async () => {
    const el = await mount();
    const links = el.shadowRoot!.querySelectorAll("a[href]");
    const hrefs = Array.from(links).map((a) => a.getAttribute("href"));
    expect(hrefs).toContain("/parent/home");
    expect(hrefs).toContain("/parent/playlists");
    expect(hrefs).toContain("/parent/activity");
    expect(hrefs).toContain("/parent/family");
    expect(hrefs).toContain("/parent/system");
  });

  it("renders an account-selector for children", async () => {
    const el = await mount();
    const selector = el.shadowRoot!.querySelector("hometube-account-selector");
    expect(selector).not.toBeNull();
    expect(selector!.getAttribute("account-type")).toBe("child");
  });

  it("renders notification bell and theme toggle", async () => {
    const el = await mount();
    expect(el.shadowRoot!.querySelector("hometube-notification-bell")).not.toBeNull();
    expect(el.shadowRoot!.querySelector("hometube-theme-toggle")).not.toBeNull();
  });

  it("re-dispatches account-changed as child-changed", async () => {
    const el = await mount();
    const handler = vi.fn();
    el.addEventListener("child-changed", handler);

    // Simulate account-selector firing its event
    const selector = el.shadowRoot!.querySelector("hometube-account-selector")!;
    selector.dispatchEvent(
      new CustomEvent("account-changed", {
        detail: { accountId: 42 },
        bubbles: true,
        composed: true,
      }),
    );

    expect(handler).toHaveBeenCalledOnce();
    expect((handler.mock.calls[0][0] as CustomEvent).detail).toEqual({ childId: 42 });
  });

  it("renders the HomeTube brand link", async () => {
    const el = await mount();
    const brand = el.shadowRoot!.querySelector('a[href="/parent/home"].brand');
    expect(brand).not.toBeNull();
    expect(brand!.textContent).toBe("HomeTube");
  });
});
