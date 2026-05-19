/**
 * Tests for `<hometube-allowlist-manager>`.
 *
 * Stubs fetch for API calls and exercises tab switching, search,
 * item listing, and add/remove flows.
 */
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import "./allowlist-manager.js";
import type { AllowlistManager } from "./allowlist-manager.js";
import { flushAsync } from "../test-utils.js";

let fetchSpy: ReturnType<typeof vi.fn>;

beforeEach(() => {
  fetchSpy = vi.fn();
  vi.stubGlobal("fetch", fetchSpy);
});

afterEach(() => {
  document.body.querySelectorAll("hometube-allowlist-manager").forEach((el) => el.remove());
  vi.restoreAllMocks();
  vi.unstubAllGlobals();
});

function mockFetch(responses: Record<string, unknown>): void {
  fetchSpy.mockImplementation((url: string) => {
    for (const [pattern, body] of Object.entries(responses)) {
      if (url.includes(pattern)) {
        return Promise.resolve({
          ok: true,
          status: 200,
          headers: new Headers({ "content-type": "application/json" }),
          json: () => Promise.resolve(body),
          text: () => Promise.resolve(JSON.stringify(body)),
        });
      }
    }
    return Promise.resolve({
      ok: true,
      status: 200,
      headers: new Headers({ "content-type": "application/json" }),
      json: () => Promise.resolve([]),
      text: () => Promise.resolve("[]"),
    });
  });
}

async function mount(childId?: number): Promise<AllowlistManager> {
  const el = document.createElement("hometube-allowlist-manager") as AllowlistManager;
  if (childId != null) {
    el.setAttribute("child-id", String(childId));
  }
  document.body.appendChild(el);
  await el.updateComplete;
  // Wait for initial data fetch
  await flushAsync(el);
  return el;
}

describe("<hometube-allowlist-manager>", () => {
  it("shows empty message when no child-id is set", async () => {
    mockFetch({});
    const el = await mount();
    const empty = el.shadowRoot!.querySelector(".empty");
    expect(empty).not.toBeNull();
    expect(empty!.textContent).toContain("Pick a child");
  });

  it("renders three tabs when child-id is set", async () => {
    mockFetch({
      "allowlist/channels": [],
      "allowlist/playlists": [],
      "allowlist/videos": [],
    });
    const el = await mount(1);
    const tabs = el.shadowRoot!.querySelectorAll('[role="tab"]');
    expect(tabs.length).toBe(3);
    const labels = Array.from(tabs).map((t) => t.textContent!.trim());
    expect(labels).toEqual(["Channels", "Playlists", "Videos"]);
  });

  it("channels tab is selected by default", async () => {
    mockFetch({
      "allowlist/channels": [],
      "allowlist/playlists": [],
      "allowlist/videos": [],
    });
    const el = await mount(1);
    const tabs = el.shadowRoot!.querySelectorAll('[role="tab"]');
    const selected = Array.from(tabs).find((t) => t.getAttribute("aria-selected") === "true");
    expect(selected!.textContent!.trim()).toBe("Channels");
  });

  it("renders a search input without a submit button", async () => {
    mockFetch({
      "allowlist/channels": [],
      "allowlist/playlists": [],
      "allowlist/videos": [],
    });
    const el = await mount(1);
    const input = el.shadowRoot!.querySelector('input[type="search"]') as HTMLInputElement;
    expect(input).not.toBeNull();
    expect(input.placeholder).toContain("Search channels");

    // The dedicated Search button was removed once typing auto-fires
    // a debounced search; the input's parent row should contain only
    // the input element.
    const row = input.parentElement!;
    expect(row.querySelector("wa-button")).toBeNull();
    expect(row.querySelector("button")).toBeNull();
  });

  it("fetches allowlist data on mount with child-id", async () => {
    mockFetch({
      "allowlist/channels": [
        { id: "ch1", channel_id: "UC1", title: "Channel One", thumbnail_url: null },
      ],
      "allowlist/playlists": [],
      "allowlist/videos": [],
    });
    await mount(2);

    // Check that channels were fetched
    expect(fetchSpy).toHaveBeenCalled();
    const urls = fetchSpy.mock.calls.map((c: string[]) => c[0]);
    expect(urls.some((u: string) => u.includes("/api/children/2/allowlist/channels"))).toBe(true);
  });

  it("switches tabs and updates search placeholder", async () => {
    mockFetch({
      "allowlist/channels": [],
      "allowlist/playlists": [],
      "allowlist/videos": [],
    });
    const el = await mount(1);

    // Click the "Videos" tab
    const tabs = el.shadowRoot!.querySelectorAll('[role="tab"]');
    const videoTab = Array.from(tabs).find((t) => t.textContent!.trim() === "Videos");
    (videoTab as HTMLElement).click();
    await el.updateComplete;

    const input = el.shadowRoot!.querySelector('input[type="search"]') as HTMLInputElement;
    expect(input.placeholder).toContain("Search videos");
  });

  it("renders allowlisted channels as cards", async () => {
    mockFetch({
      "allowlist/channels": [
        {
          id: 1,
          channel_id: "UC1",
          channel_title: "Test Channel",
          channel_thumbnail_url: "https://img.test/t.jpg",
        },
        { id: 2, channel_id: "UC2", channel_title: "Another Channel", channel_thumbnail_url: null },
      ],
      "allowlist/playlists": [],
      "allowlist/videos": [],
    });
    const el = await mount(1);

    const cards = el.shadowRoot!.querySelectorAll("hometube-content-card");
    expect(cards.length).toBe(2);
    expect(cards[0].getAttribute("title")).toBe("Test Channel");
  });

  it("shows empty message when no channels exist", async () => {
    mockFetch({
      "allowlist/channels": [],
      "allowlist/playlists": [],
      "allowlist/videos": [],
    });
    const el = await mount(1);

    const empty = el.shadowRoot!.querySelector(".empty");
    expect(empty).not.toBeNull();
    expect(empty!.textContent).toContain("No channels yet");
  });

  it("performs search and displays results after debounce", async () => {
    mockFetch({
      "allowlist/channels": [],
      "allowlist/playlists": [],
      "allowlist/videos": [],
      "parent/search": {
        items: [
          {
            id: "UCabc",
            kind: "channel",
            title: "Found Channel",
            description: "Desc",
            channel_title: null,
            thumbnails: { default: { url: "https://img.test/t.jpg" } },
          },
        ],
      },
    });
    vi.useFakeTimers({ toFake: ["setTimeout", "clearTimeout"] });
    try {
      const el = await mount(1);

      // Type in search — the debounced auto-search is the only entry
      // point now that the Search button has been removed.
      const input = el.shadowRoot!.querySelector('input[type="search"]') as HTMLInputElement;
      input.value = "test query";
      input.dispatchEvent(new Event("input", { bubbles: true }));

      // Advance past the debounce window so the request fires.
      vi.advanceTimersByTime(350);
      await flushAsync(el);

      // Should show search results
      const headings = el.shadowRoot!.querySelectorAll("h3");
      const addHeading = Array.from(headings).find((h) => h.textContent!.includes("Add a result"));
      expect(addHeading).not.toBeNull();
    } finally {
      vi.useRealTimers();
    }
  });

  it("auto-searches after typing once the debounce elapses", async () => {
    mockFetch({
      "allowlist/channels": [],
      "allowlist/playlists": [],
      "allowlist/videos": [],
      "parent/search": { items: [] },
    });
    // Install fake timers before mount so any deferred work scheduled
    // during connectedCallback is also controllable.
    vi.useFakeTimers({ toFake: ["setTimeout", "clearTimeout"] });
    try {
      const el = await mount(1);
      const callsBefore = fetchSpy.mock.calls.length;

      // Type without pressing Enter / clicking the button.
      const input = el.shadowRoot!.querySelector('input[type="search"]') as HTMLInputElement;
      input.value = "kittens";
      input.dispatchEvent(new Event("input", { bubbles: true }));

      // Immediately afterwards the request has not yet been made.
      let searchCalls = fetchSpy.mock.calls
        .slice(callsBefore)
        .filter((c: string[]) => c[0].includes("parent/search"));
      expect(searchCalls.length).toBe(0);

      // Advance past the 300 ms debounce window deterministically.
      vi.advanceTimersByTime(350);
      await flushAsync(el);

      searchCalls = fetchSpy.mock.calls
        .slice(callsBefore)
        .filter((c: string[]) => c[0].includes("parent/search"));
      expect(searchCalls.length).toBe(1);
      expect(searchCalls[0][0]).toContain("q=kittens");
    } finally {
      vi.useRealTimers();
    }
  });

  it("collapses rapid keystrokes into a single debounced request", async () => {
    mockFetch({
      "allowlist/channels": [],
      "allowlist/playlists": [],
      "allowlist/videos": [],
      "parent/search": { items: [] },
    });
    vi.useFakeTimers({ toFake: ["setTimeout", "clearTimeout"] });
    try {
      const el = await mount(1);
      const callsBefore = fetchSpy.mock.calls.length;

      const input = el.shadowRoot!.querySelector('input[type="search"]') as HTMLInputElement;
      for (const value of ["k", "ki", "kit", "kitt", "kitte", "kitten", "kittens"]) {
        input.value = value;
        input.dispatchEvent(new Event("input", { bubbles: true }));
        // Advance 20 ms between keystrokes — well inside the 300 ms debounce.
        vi.advanceTimersByTime(20);
      }
      vi.advanceTimersByTime(350);
      await flushAsync(el);

      const searchCalls = fetchSpy.mock.calls
        .slice(callsBefore)
        .filter((c: string[]) => c[0].includes("parent/search"));
      expect(searchCalls.length).toBe(1);
      expect(searchCalls[0][0]).toContain("q=kittens");
    } finally {
      vi.useRealTimers();
    }
  });

  it("clearing the search box cancels any pending debounced request", async () => {
    mockFetch({
      "allowlist/channels": [],
      "allowlist/playlists": [],
      "allowlist/videos": [],
      "parent/search": { items: [] },
    });
    vi.useFakeTimers({ toFake: ["setTimeout", "clearTimeout"] });
    try {
      const el = await mount(1);
      const callsBefore = fetchSpy.mock.calls.length;

      const input = el.shadowRoot!.querySelector('input[type="search"]') as HTMLInputElement;
      input.value = "kittens";
      input.dispatchEvent(new Event("input", { bubbles: true }));
      // Clear immediately, before the debounce fires.
      input.value = "";
      input.dispatchEvent(new Event("input", { bubbles: true }));

      vi.advanceTimersByTime(350);
      await flushAsync(el);

      const searchCalls = fetchSpy.mock.calls
        .slice(callsBefore)
        .filter((c: string[]) => c[0].includes("parent/search"));
      expect(searchCalls.length).toBe(0);
    } finally {
      vi.useRealTimers();
    }
  });

  it("does not search with empty query", async () => {
    mockFetch({
      "allowlist/channels": [],
      "allowlist/playlists": [],
      "allowlist/videos": [],
    });
    const el = await mount(1);

    const callsBefore = fetchSpy.mock.calls.length;

    // Fire an input event with an empty value. The debounce path
    // should bail out without scheduling a request.
    vi.useFakeTimers({ toFake: ["setTimeout", "clearTimeout"] });
    try {
      const input = el.shadowRoot!.querySelector('input[type="search"]') as HTMLInputElement;
      input.value = "";
      input.dispatchEvent(new Event("input", { bubbles: true }));
      vi.advanceTimersByTime(350);
      await flushAsync(el);
    } finally {
      vi.useRealTimers();
    }

    const searchCalls = fetchSpy.mock.calls
      .slice(callsBefore)
      .filter((c: string[]) => c[0].includes("parent/search"));
    expect(searchCalls.length).toBe(0);
  });

  it("shows error banner on API failure", async () => {
    fetchSpy.mockImplementation((url: string) => {
      if (url.includes("allowlist/channels")) {
        return Promise.resolve({
          ok: false,
          status: 500,
          headers: new Headers({ "content-type": "application/json" }),
          json: () => Promise.resolve("Server error"),
          text: () => Promise.resolve('"Server error"'),
        });
      }
      return Promise.resolve({
        ok: true,
        status: 200,
        headers: new Headers({ "content-type": "application/json" }),
        json: () => Promise.resolve([]),
        text: () => Promise.resolve("[]"),
      });
    });
    const mounted = await mount(1);

    const errorBanner = mounted.shadowRoot!.querySelector("hometube-error-banner");
    expect(errorBanner).not.toBeNull();
  });

  it("renders remove button for allowlisted items", async () => {
    mockFetch({
      "allowlist/channels": [
        { id: 1, channel_id: "UC1", channel_title: "Ch1", channel_thumbnail_url: null },
      ],
      "allowlist/playlists": [],
      "allowlist/videos": [],
    });
    const el = await mount(1);

    const card = el.shadowRoot!.querySelector("hometube-content-card");
    expect(card).not.toBeNull();
    const removeBtn = card!.querySelector("wa-button");
    expect(removeBtn).not.toBeNull();
    expect(removeBtn!.textContent).toContain("Remove");
  });

  it("calls remove API when Remove button is clicked", async () => {
    mockFetch({
      "allowlist/channels": [
        { id: 1, channel_id: "UC1", channel_title: "Ch1", channel_thumbnail_url: null },
      ],
      "allowlist/playlists": [],
      "allowlist/videos": [],
    });
    const el = await mount(1);

    const card = el.shadowRoot!.querySelector("hometube-content-card");
    const removeBtn = card!.querySelector("wa-button")! as HTMLElement;
    removeBtn.click();
    await flushAsync(el);

    const deleteCalls = fetchSpy.mock.calls.filter(
      (c: unknown[]) =>
        (c[0] as string).includes("/allowlist/channels/UC1") &&
        (c[1] as RequestInit | undefined)?.method === "DELETE",
    );
    expect(deleteCalls.length).toBe(1);
  });
});
