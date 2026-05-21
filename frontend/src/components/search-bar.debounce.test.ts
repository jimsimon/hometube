/**
 * Regression test for the child-page search bar debounce.
 *
 * The previous 200ms window was shorter than a typical keystroke
 * interval (~50 WPM ≈ 220ms/char), so the trailing-edge timer expired
 * between characters and every keystroke fired its own `/api/search`
 * request. The window was bumped to 300ms; this test fails if it
 * regresses below ~250ms.
 *
 * Uses fake timers so the test is deterministic (and ~instant) under
 * CI load. Lit drives its updates via microtasks, not timers, so
 * `el.updateComplete` continues to work alongside `vi.useFakeTimers`.
 */
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import "./search-bar.js";
import type { SearchBar } from "./search-bar.js";

function mockFetch() {
  const spy = vi.fn().mockImplementation(
    () =>
      Promise.resolve({
        ok: true,
        status: 200,
        headers: new Headers({ "content-type": "application/json" }),
        json: () =>
          Promise.resolve({
            q: "",
            kind: "all",
            results: { channels: [], playlists: [], videos: [] },
            next_page_token: null,
          }),
        text: () => Promise.resolve("{}"),
      }) as unknown as Promise<Response>,
  );
  vi.stubGlobal("fetch", spy);
  return spy;
}

function stubDesktopMatchMedia() {
  // Force `(max-width: 48rem)` to be false so the bar renders its
  // desktop (non-compact) layout — i.e. the <input> is in the DOM
  // immediately, without needing to click the expand button.
  vi.stubGlobal(
    "matchMedia",
    (query: string): MediaQueryList =>
      ({
        matches: false,
        media: query,
        onchange: null,
        addEventListener: () => {},
        removeEventListener: () => {},
        // Legacy listeners — Lit/wa-components don't use these, but
        // some libs still check for them.
        addListener: () => {},
        removeListener: () => {},
        dispatchEvent: () => false,
      }) as unknown as MediaQueryList,
  );
}

let fetchSpy: ReturnType<typeof mockFetch>;

beforeEach(() => {
  stubDesktopMatchMedia();
  fetchSpy = mockFetch();
  vi.useFakeTimers({ toFake: ["setTimeout", "clearTimeout"] });
});

afterEach(() => {
  document.body.querySelectorAll("hometube-search-bar").forEach((el) => el.remove());
  vi.useRealTimers();
  vi.unstubAllGlobals();
  vi.restoreAllMocks();
});

async function mountBar(): Promise<SearchBar> {
  const el = document.createElement("hometube-search-bar") as SearchBar;
  document.body.appendChild(el);
  await el.updateComplete;
  return el;
}

function typeChar(el: SearchBar, value: string) {
  const input = el.shadowRoot!.querySelector<HTMLInputElement>("#search-input")!;
  input.value = value;
  input.dispatchEvent(new Event("input"));
}

const apiSearchCalls = () =>
  fetchSpy.mock.calls.filter((c) => String(c[0] ?? "").includes("/api/search")).length;

describe("<hometube-search-bar> debounce", () => {
  it("coalesces a 7-char word typed at ~50 WPM into a single /api/search", async () => {
    const el = await mountBar();
    const word = "kittens";
    // 220ms per char ≈ 50 WPM — slightly below average typing speed.
    // The debounce window must be longer than this to actually coalesce.
    const GAP_MS = 220;
    for (let i = 1; i <= word.length; i++) {
      typeChar(el, word.slice(0, i));
      await vi.advanceTimersByTimeAsync(GAP_MS);
    }
    // Settle: wait long enough for the trailing call to fire.
    await vi.advanceTimersByTimeAsync(500);

    expect(apiSearchCalls()).toBe(1);
  });
});
