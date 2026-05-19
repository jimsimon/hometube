/**
 * Tests for `<hometube-search-results>`.
 *
 * Focused on the in-page `search-change` event handling that was
 * introduced alongside the debounced child search bar: kind
 * validation, empty-query clearing, URL `replaceState` behavior,
 * query length cap, the cancelable event flow, and the no-op early
 * return when q/type are unchanged.
 */
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import "./search-results.js";
import type { SearchResults } from "./search-results.js";

let fetchSpy: ReturnType<typeof vi.fn>;

const EMPTY_SEARCH_BODY = {
  q: "",
  kind: "all",
  results: { channels: [], playlists: [], videos: [] },
  next_page_token: null,
};

function mockFetch(): void {
  fetchSpy = vi.fn().mockImplementation(() =>
    Promise.resolve({
      ok: true,
      status: 200,
      headers: new Headers({ "content-type": "application/json" }),
      json: () => Promise.resolve(EMPTY_SEARCH_BODY),
      text: () => Promise.resolve(JSON.stringify(EMPTY_SEARCH_BODY)),
    }),
  );
  vi.stubGlobal("fetch", fetchSpy);
}

async function mount(q = "", type = "all"): Promise<SearchResults> {
  const el = document.createElement("hometube-search-results") as SearchResults;
  el.q = q;
  el.type = type as SearchResults["type"];
  document.body.appendChild(el);
  await el.updateComplete;
  // Allow any kicked-off fetch to settle.
  await new Promise((r) => setTimeout(r, 10));
  await el.updateComplete;
  return el;
}

function searchCallsFor(spy: ReturnType<typeof vi.fn>): string[] {
  return spy.mock.calls
    .map((c: unknown[]) => c[0] as string)
    .filter((u) => u.includes("/api/search"));
}

function dispatchChange(
  el: HTMLElement,
  detail: { q: string; kind: string },
  cancelable = true,
): CustomEvent {
  const event = new CustomEvent("search-change", { detail, cancelable });
  const bar = el.shadowRoot!.querySelector("hometube-search-bar")!;
  bar.dispatchEvent(event);
  return event;
}

beforeEach(() => {
  mockFetch();
  // Reset URL between tests so replaceState assertions are stable.
  window.history.replaceState(null, "", "/child/search");
});

afterEach(() => {
  document.body.querySelectorAll("hometube-search-results").forEach((el) => el.remove());
  vi.restoreAllMocks();
  vi.unstubAllGlobals();
});

describe("<hometube-search-results> onSearchChange", () => {
  it("issues only one initial fetch on mount", async () => {
    const el = await mount("cats", "video");
    expect(searchCallsFor(fetchSpy).length).toBe(1);
    expect(el.q).toBe("cats");
  });

  it("updates state and URL when the embedded bar emits search-change", async () => {
    const el = await mount("cats", "all");
    fetchSpy.mockClear();

    dispatchChange(el, { q: "dogs", kind: "video" });
    await el.updateComplete;
    await new Promise((r) => setTimeout(r, 10));

    expect(el.q).toBe("dogs");
    expect(el.type).toBe("video");
    const url = new URL(window.location.href);
    expect(url.searchParams.get("q")).toBe("dogs");
    expect(url.searchParams.get("type")).toBe("video");
    expect(searchCallsFor(fetchSpy).length).toBe(1);
  });

  it("falls back to 'all' when the kind is not one of the known union values", async () => {
    const el = await mount("cats", "all");

    dispatchChange(el, { q: "cats", kind: "evil-injection" });
    await el.updateComplete;

    expect(el.type).toBe("all");
  });

  it("clears stored results and loading state when the query is cleared", async () => {
    const el = await mount("cats", "all");
    // Force some state as if a previous search had populated it.
    (el as unknown as { channels: unknown[] }).channels = [{ a: 1 }];
    (el as unknown as { loading: boolean }).loading = true;

    dispatchChange(el, { q: "", kind: "all" });
    await el.updateComplete;

    expect(el.q).toBe("");
    expect((el as unknown as { channels: unknown[] }).channels).toEqual([]);
    expect((el as unknown as { playlists: unknown[] }).playlists).toEqual([]);
    expect((el as unknown as { videos: unknown[] }).videos).toEqual([]);
    expect((el as unknown as { loading: boolean }).loading).toBe(false);

    const url = new URL(window.location.href);
    expect(url.searchParams.get("q")).toBeNull();
  });

  it("caps very long queries before they hit the URL or API", async () => {
    const el = await mount("cats", "all");
    fetchSpy.mockClear();

    const huge = "x".repeat(5000);
    dispatchChange(el, { q: huge, kind: "all" });
    await el.updateComplete;
    await new Promise((r) => setTimeout(r, 10));

    expect(el.q.length).toBe(200);
    const url = new URL(window.location.href);
    expect(url.searchParams.get("q")!.length).toBe(200);
    const fetched = searchCallsFor(fetchSpy);
    expect(fetched.length).toBe(1);
    // The request URL also reflects the cap (URL-encoded length == 200
    // since all characters are ASCII single-byte).
    const requestQ = new URL(fetched[0], window.location.origin).searchParams.get("q");
    expect(requestQ!.length).toBe(200);
  });

  it("calls preventDefault on the cancelable event so the bar skips navigation", async () => {
    const el = await mount("cats", "all");
    const event = dispatchChange(el, { q: "dogs", kind: "all" }, true);
    expect(event.defaultPrevented).toBe(true);
  });

  it("is a no-op when q and type are unchanged", async () => {
    const el = await mount("cats", "all");
    fetchSpy.mockClear();
    const urlBefore = window.location.href;

    dispatchChange(el, { q: "cats", kind: "all" });
    await el.updateComplete;
    await new Promise((r) => setTimeout(r, 10));

    expect(searchCallsFor(fetchSpy).length).toBe(0);
    expect(window.location.href).toBe(urlBefore);
  });
});
