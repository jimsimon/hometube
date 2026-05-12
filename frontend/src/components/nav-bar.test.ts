/**
 * Smoke tests for `<hometube-nav-bar>`.
 *
 * Exercise the slot rendering + the optional display-name + the
 * built-in profile/logout controls — the three concerns the
 * component owns after T-9.
 */
import { afterEach, describe, expect, it } from "vitest";

import "./nav-bar.js";

afterEach(() => {
  document.body.querySelectorAll("hometube-nav-bar").forEach((el) => el.remove());
});

async function mount(markup: string): Promise<HTMLElement> {
  document.body.insertAdjacentHTML("beforeend", markup);
  const el = document.body.lastElementChild as HTMLElement;
  await (el as HTMLElement & { updateComplete?: Promise<unknown> }).updateComplete;
  return el;
}

describe("<hometube-nav-bar>", () => {
  it("renders brand, primary, and actions slots", async () => {
    const el = await mount(`
      <hometube-nav-bar nav-label="Test nav">
        <span slot="brand">Brand</span>
        <span slot="primary">Primary</span>
        <span slot="actions">Action</span>
      </hometube-nav-bar>
    `);
    const nav = el.shadowRoot!.querySelector("nav");
    expect(nav).not.toBeNull();
    expect(nav!.getAttribute("aria-label")).toBe("Test nav");
    const slots = el.shadowRoot!.querySelectorAll("slot");
    const names = Array.from(slots).map((s) => s.getAttribute("name"));
    expect(names).toEqual(expect.arrayContaining(["brand", "primary", "actions"]));
  });

  it("renders the display-name and Switch profile link by default", async () => {
    const el = await mount(`
      <hometube-nav-bar display-name="Alex"></hometube-nav-bar>
    `);
    // textContent strips out Lit dev-mode comment markers so a
    // substring match against the visible text is reliable.
    const text = el.shadowRoot!.textContent ?? "";
    expect(text).toContain("Alex");
    expect(text).toContain("Signed in as");
    const html = el.shadowRoot!.innerHTML;
    expect(html).toContain('href="/profiles"');
    expect(html).toContain("Log out");
  });

  it("hides the profile link and logout button when requested", async () => {
    const el = await mount(`
      <hometube-nav-bar hide-profile hide-logout></hometube-nav-bar>
    `);
    const html = el.shadowRoot!.innerHTML;
    expect(html).not.toContain('href="/profiles"');
    expect(html).not.toContain("Log out");
  });
});
