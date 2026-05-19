/**
 * Smoke tests for `<hometube-account-selector>`.
 *
 * Mocks `/api/accounts*` via `fetch` and asserts the component
 * renders a `<select>` in single mode and a checkbox group in
 * multi-select mode, plus that selecting fires the bubbling
 * `account-changed` event with the right payload.
 */
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import "./account-selector.js";
import { flushAsync } from "../test-utils.js";

type WithUpdate = HTMLElement & { updateComplete?: Promise<boolean> };

const ACCOUNTS = [
  { id: 1, display_name: "Alice", account_type: "child" },
  { id: 2, display_name: "Bob", account_type: "child" },
];

async function mount(markup: string): Promise<HTMLElement> {
  document.body.insertAdjacentHTML("beforeend", markup);
  const el = document.body.lastElementChild as HTMLElement;
  // Wait for connectedCallback to run + the network round-trip to
  // resolve before reading the shadow DOM.
  const withUpdate = el as WithUpdate;
  if (withUpdate.updateComplete) await withUpdate.updateComplete;
  // Pass the element so flushAsync's `updateComplete` loop also waits
  // for the post-fetch render to settle, not just for microtasks.
  await flushAsync(withUpdate);
  return el;
}

describe("<hometube-account-selector>", () => {
  beforeEach(() => {
    localStorage.clear();
    vi.stubGlobal(
      "fetch",
      vi.fn(async () => {
        return new Response(JSON.stringify(ACCOUNTS), {
          status: 200,
          headers: { "Content-Type": "application/json" },
        });
      }),
    );
  });

  afterEach(() => {
    document.body.querySelectorAll("hometube-account-selector").forEach((el) => el.remove());
    vi.unstubAllGlobals();
  });

  it("renders a single-select with one option per account", async () => {
    const el = await mount(
      `<hometube-account-selector account-type="child"></hometube-account-selector>`,
    );
    const select = el.shadowRoot!.querySelector("select");
    expect(select).not.toBeNull();
    const options = el.shadowRoot!.querySelectorAll("option");
    expect(options).toHaveLength(2);
    expect(options[0]!.textContent?.trim()).toBe("Alice");
  });

  it("emits account-changed on initial load with the first ID", async () => {
    const events: number[] = [];
    document.body.addEventListener(
      "account-changed",
      (e: Event) => events.push((e as CustomEvent).detail.accountId),
      { once: true },
    );
    await mount(`<hometube-account-selector account-type="child"></hometube-account-selector>`);
    // The selector emits during its initial load; flush microtasks so
    // the listener has fired by the time we assert.
    await flushAsync();
    expect(events).toContain(1);
  });

  it("multi-mode renders a checkbox per account", async () => {
    const el = await mount(`
      <hometube-account-selector multiple account-type="child"></hometube-account-selector>
    `);
    const boxes = el.shadowRoot!.querySelectorAll('input[type="checkbox"]');
    expect(boxes).toHaveLength(2);
  });
});
