/**
 * Lit component test for `<hometube-theme-toggle>`.
 *
 * Demonstrates the @vitest/browser pattern: import the component to
 * trigger the `@customElement` registration, mount it into the DOM,
 * and assert against shadow-DOM behaviour.
 */
import { afterEach, beforeEach, describe, expect, it } from "vitest";

import "./theme-toggle.js";

describe("<hometube-theme-toggle>", () => {
  let host: HTMLElement;

  beforeEach(() => {
    localStorage.clear();
    document.documentElement.className = "";
    host = document.createElement("hometube-theme-toggle");
    document.body.appendChild(host);
  });

  afterEach(() => {
    host.remove();
    document.documentElement.className = "";
    localStorage.clear();
  });

  function getSelect(): HTMLSelectElement {
    const sr = host.shadowRoot;
    if (!sr) throw new Error("no shadow root");
    const select = sr.querySelector("select");
    if (!select) throw new Error("select not rendered");
    return select;
  }

  it("renders a select with three options", async () => {
    await (host as HTMLElement & { updateComplete?: Promise<unknown> }).updateComplete;
    const select = getSelect();
    expect(select.options.length).toBe(3);
    const values = Array.from(select.options).map((o) => o.value);
    expect(values).toEqual(["system", "light", "dark"]);
  });

  it("persists the selected theme on change", async () => {
    await (host as HTMLElement & { updateComplete?: Promise<unknown> }).updateComplete;
    const select = getSelect();
    select.value = "dark";
    select.dispatchEvent(new Event("change"));

    expect(localStorage.getItem("hometube-theme")).toBe("dark");
    expect(document.documentElement.classList.contains("wa-dark")).toBe(true);
  });

  it("reflects the persisted theme on mount", async () => {
    localStorage.setItem("hometube-theme", "light");
    const fresh = document.createElement("hometube-theme-toggle");
    document.body.appendChild(fresh);
    await (fresh as HTMLElement & { updateComplete?: Promise<unknown> }).updateComplete;
    const sr = fresh.shadowRoot;
    expect(sr).not.toBeNull();
    const select = sr!.querySelector("select") as HTMLSelectElement;
    expect(select.value).toBe("light");
    fresh.remove();
  });
});
