/**
 * Tests for `<hometube-preview-channel>`.
 *
 * Stubs fetch and exercises the read-only grid plus the per-video
 * allowlist picker that appears when `child-id` is set.
 */
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import "./preview-channel.js";
import type { PreviewChannel } from "./preview-channel.js";
import { flushAsync } from "../test-utils.js";

let fetchSpy: ReturnType<typeof vi.fn>;

beforeEach(() => {
  fetchSpy = vi.fn();
  vi.stubGlobal("fetch", fetchSpy);
});

afterEach(() => {
  document.body.querySelectorAll("hometube-preview-channel").forEach((el) => el.remove());
  vi.restoreAllMocks();
  vi.unstubAllGlobals();
});

function jsonResponse(body: unknown, status = 200) {
  return Promise.resolve({
    ok: status >= 200 && status < 300,
    status,
    headers: new Headers({ "content-type": "application/json" }),
    json: () => Promise.resolve(body),
    text: () => Promise.resolve(JSON.stringify(body)),
  });
}

const CHANNEL = {
  id: "UCabc",
  title: "Dino Facts",
  description: "All about dinosaurs",
  thumbnails: { default: { url: "https://img.test/avatar.jpg" } },
  video_count: 2,
  videos: [
    {
      video_id: "vid-1",
      title: "T-Rex",
      channel_id: "UCabc",
      channel_title: "Dino Facts",
      thumbnails: { high: { url: "https://img.test/1.jpg" } },
      published_at: "2d ago",
      position: null,
    },
    {
      video_id: "vid-2",
      title: "Stegosaurus",
      channel_id: "UCabc",
      channel_title: null,
      thumbnails: {},
      published_at: null,
      position: null,
    },
    {
      video_id: "vid-3",
      title: "Triceratops",
      channel_id: "UCabc",
      channel_title: "Dino Facts",
      thumbnails: {},
      published_at: null,
      position: null,
    },
  ],
  next_page_token: null,
};

/** Default stub: channel preview + an allowlist that already has vid-3. */
function mockDefault(opts: { failAdd?: string[] } = {}): void {
  fetchSpy.mockImplementation((url: string, init?: RequestInit) => {
    if (url.includes("/api/preview/channel/")) return jsonResponse(CHANNEL);
    if (url.includes("/allowlist/videos") && init?.method === "POST") {
      const body = JSON.parse(String(init.body)) as { video_id: string };
      if (opts.failAdd?.includes(body.video_id)) return jsonResponse("boom", 500);
      return jsonResponse({ id: 1, video_id: body.video_id });
    }
    if (url.includes("/allowlist/videos")) {
      return jsonResponse([{ id: 9, video_id: "vid-3", video_title: "Triceratops" }]);
    }
    return jsonResponse([]);
  });
}

async function mount(childId?: number): Promise<PreviewChannel> {
  const el = document.createElement("hometube-preview-channel") as PreviewChannel;
  el.setAttribute("channel-id", "UCabc");
  if (childId != null) el.setAttribute("child-id", String(childId));
  document.body.appendChild(el);
  await el.updateComplete;
  await flushAsync(el);
  return el;
}

function postedIds(): string[] {
  return fetchSpy.mock.calls
    .filter(
      (c: unknown[]) =>
        (c[0] as string).includes("/allowlist/videos") &&
        (c[1] as RequestInit | undefined)?.method === "POST",
    )
    .map(
      (c: unknown[]) =>
        (JSON.parse(String((c[1] as RequestInit).body)) as { video_id: string }).video_id,
    );
}

describe("<hometube-preview-channel>", () => {
  it("renders the channel header and one card per upload", async () => {
    mockDefault();
    const el = await mount();
    const root = el.shadowRoot!;
    expect(root.querySelector("h1")!.textContent).toContain("Dino Facts");
    const cards = root.querySelectorAll(".card");
    expect(cards.length).toBe(3);
    // Cards link to the parent watch page.
    expect(cards[0].querySelector("a")!.getAttribute("href")).toBe("/parent/preview/video/vid-1");
  });

  it("is read-only without a child-id", async () => {
    mockDefault();
    const el = await mount();
    const root = el.shadowRoot!;
    expect(root.querySelector('input[type="checkbox"]')).toBeNull();
    expect(root.querySelector("wa-button")).toBeNull();
    // No allowlist lookup either.
    const allowlistCalls = fetchSpy.mock.calls.filter((c: unknown[]) =>
      (c[0] as string).includes("/allowlist/videos"),
    );
    expect(allowlistCalls.length).toBe(0);
  });

  it("shows the picker with child-id and marks already-allowlisted videos", async () => {
    mockDefault();
    const el = await mount(4);
    const root = el.shadowRoot!;
    // Fetched the child's current allowlist.
    expect(
      fetchSpy.mock.calls.some((c: unknown[]) =>
        (c[0] as string).includes("/api/children/4/allowlist/videos"),
      ),
    ).toBe(true);
    // Two addable cards get a checkbox + Add; the allowlisted one shows Added.
    expect(root.querySelectorAll('input[type="checkbox"]').length).toBe(2);
    expect(root.querySelectorAll("wa-button.add-one").length).toBe(2);
    const added = root.querySelector('.card[data-video-id="vid-3"] .added');
    expect(added).not.toBeNull();
    expect(added!.textContent).toContain("Added");
  });

  it("adds a single video with its metadata and emits allowlist-videos-added", async () => {
    mockDefault();
    const el = await mount(4);
    const events: CustomEvent[] = [];
    el.addEventListener("allowlist-videos-added", (e) => events.push(e as CustomEvent));

    const addBtn = el.shadowRoot!.querySelector(
      '.card[data-video-id="vid-1"] wa-button.add-one',
    ) as HTMLElement;
    addBtn.click();
    await flushAsync(el);

    const posts = fetchSpy.mock.calls.filter(
      (c: unknown[]) =>
        (c[0] as string) === "/api/children/4/allowlist/videos" &&
        (c[1] as RequestInit | undefined)?.method === "POST",
    );
    expect(posts.length).toBe(1);
    const body = JSON.parse(String((posts[0][1] as RequestInit).body));
    expect(body).toEqual({
      video_id: "vid-1",
      title: "T-Rex",
      channel_title: "Dino Facts",
      thumbnail_url: "https://img.test/1.jpg",
    });

    expect(events.length).toBe(1);
    expect(events[0].detail).toEqual({ childId: 4, videoIds: ["vid-1"] });
    // The card now reads Added and lost its controls.
    const card = el.shadowRoot!.querySelector('.card[data-video-id="vid-1"]')!;
    expect(card.querySelector(".added")).not.toBeNull();
    expect(card.querySelector("wa-button.add-one")).toBeNull();
  });

  it("adds every selected video in one batch and falls back to the channel title", async () => {
    mockDefault();
    const el = await mount(4);
    const root = el.shadowRoot!;

    const addSelected = root.querySelector("wa-button.add-selected") as HTMLElement & {
      disabled?: boolean;
    };
    expect(addSelected.hasAttribute("disabled")).toBe(true);

    for (const id of ["vid-1", "vid-2"]) {
      const box = root.querySelector(
        `.card[data-video-id="${id}"] input[type="checkbox"]`,
      ) as HTMLInputElement;
      box.checked = true;
      box.dispatchEvent(new Event("change", { bubbles: true }));
    }
    await el.updateComplete;
    expect(root.querySelector(".toolbar .hint")!.textContent).toContain("2 selected");
    expect(addSelected.hasAttribute("disabled")).toBe(false);

    addSelected.click();
    await flushAsync(el);

    expect(postedIds()).toEqual(["vid-1", "vid-2"]);
    // vid-2 has no channel_title of its own → the channel's title is used.
    const vid2Post = fetchSpy.mock.calls.find(
      (c: unknown[]) =>
        (c[1] as RequestInit | undefined)?.method === "POST" &&
        String((c[1] as RequestInit).body).includes('"vid-2"'),
    )!;
    expect(JSON.parse(String((vid2Post[1] as RequestInit).body)).channel_title).toBe("Dino Facts");
    // Everything is now marked Added; the selection is empty.
    expect(root.querySelectorAll(".added").length).toBe(3);
    expect(root.querySelectorAll('input[type="checkbox"]').length).toBe(0);
  });

  it("select all only picks videos that can still be added", async () => {
    mockDefault();
    const el = await mount(4);
    const root = el.shadowRoot!;
    const buttons = Array.from(root.querySelectorAll(".toolbar wa-button"));
    const selectAll = buttons.find((b) => b.textContent!.includes("Select all")) as HTMLElement;
    selectAll.click();
    await el.updateComplete;
    expect(root.querySelector(".toolbar .hint")!.textContent).toContain("2 selected");
    // Toggles to "Clear selection" once everything addable is picked.
    const clear = Array.from(root.querySelectorAll(".toolbar wa-button")).find((b) =>
      b.textContent!.includes("Clear selection"),
    ) as HTMLElement;
    expect(clear).toBeDefined();
    clear.click();
    await el.updateComplete;
    expect(root.querySelector(".toolbar .hint")!.textContent).toContain("Tick videos");
  });

  it("keeps failed videos selected and reports the failure", async () => {
    mockDefault({ failAdd: ["vid-2"] });
    const el = await mount(4);
    const root = el.shadowRoot!;
    const selectAll = Array.from(root.querySelectorAll(".toolbar wa-button")).find((b) =>
      b.textContent!.includes("Select all"),
    ) as HTMLElement;
    selectAll.click();
    await el.updateComplete;
    (root.querySelector("wa-button.add-selected") as HTMLElement).click();
    await flushAsync(el);

    expect(postedIds()).toEqual(["vid-1", "vid-2"]);
    expect(root.querySelector('.card[data-video-id="vid-1"] .added')).not.toBeNull();
    // vid-2 stays addable + selected, with an error banner.
    const box = root.querySelector(
      '.card[data-video-id="vid-2"] input[type="checkbox"]',
    ) as HTMLInputElement;
    expect(box).not.toBeNull();
    expect(box.checked).toBe(true);
    const banner = root.querySelector("hometube-error-banner") as HTMLElement & {
      message?: string;
    };
    expect(banner).not.toBeNull();
    expect(banner.message).toContain("Stegosaurus");
  });
});
