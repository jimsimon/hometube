/**
 * Tests for `<hometube-notification-bell>`.
 *
 * Stubs `fetch` to avoid real API calls and exercises the bell icon,
 * badge rendering, panel toggle, and item listing.
 */
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import "./notification-bell.js";
import type { NotificationBell } from "./notification-bell.js";

let fetchSpy: ReturnType<typeof vi.fn>;

beforeEach(() => {
  fetchSpy = vi.fn();
  vi.stubGlobal("fetch", fetchSpy);
});

afterEach(() => {
  document.body.querySelectorAll("hometube-notification-bell").forEach((el) => el.remove());
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
      json: () => Promise.resolve({}),
      text: () => Promise.resolve("{}"),
    });
  });
}

async function mount(): Promise<NotificationBell> {
  const el = document.createElement("hometube-notification-bell") as NotificationBell;
  document.body.appendChild(el);
  await el.updateComplete;
  // Wait for initial fetch to resolve
  await new Promise((r) => setTimeout(r, 10));
  await el.updateComplete;
  return el;
}

describe("<hometube-notification-bell>", () => {
  it("renders the bell button with aria attributes", async () => {
    mockFetch({ "unread-count": { unread: 0 } });
    const el = await mount();
    const btn = el.shadowRoot!.querySelector("button.bell");
    expect(btn).not.toBeNull();
    expect(btn!.getAttribute("aria-haspopup")).toBe("true");
    expect(btn!.getAttribute("aria-expanded")).toBe("false");
  });

  it("shows unread badge when count > 0", async () => {
    mockFetch({ "unread-count": { unread: 5 } });
    const el = await mount();
    const badge = el.shadowRoot!.querySelector(".badge");
    expect(badge).not.toBeNull();
    expect(badge!.textContent).toBe("5");
  });

  it("caps badge at 99+", async () => {
    mockFetch({ "unread-count": { unread: 150 } });
    const el = await mount();
    const badge = el.shadowRoot!.querySelector(".badge");
    expect(badge!.textContent).toBe("99+");
  });

  it("hides badge when unread is 0", async () => {
    mockFetch({ "unread-count": { unread: 0 } });
    const el = await mount();
    const badge = el.shadowRoot!.querySelector(".badge");
    expect(badge).toBeNull();
  });

  it("opens panel on click and fetches items", async () => {
    const items = [
      {
        id: 1,
        notification_type: "system_update",
        title: "Update",
        message: "System updated",
        is_read: 0,
        created_at: 1700000000,
      },
    ];
    mockFetch({
      "unread-count": { unread: 1 },
      "notifications?": items,
    });
    const el = await mount();

    const btn = el.shadowRoot!.querySelector("button.bell")!;
    btn.dispatchEvent(new Event("click", { bubbles: true }));
    await new Promise((r) => setTimeout(r, 10));
    await el.updateComplete;

    expect(btn.getAttribute("aria-expanded")).toBe("true");
    const panel = el.shadowRoot!.querySelector(".panel");
    expect(panel).not.toBeNull();
    const li = el.shadowRoot!.querySelector("li");
    expect(li).not.toBeNull();
    expect(li!.classList.contains("unread")).toBe(true);
    expect(li!.textContent).toContain("Update");
  });

  it("shows empty state when no notifications", async () => {
    mockFetch({
      "unread-count": { unread: 0 },
      "notifications?": [],
    });
    const el = await mount();

    const btn = el.shadowRoot!.querySelector("button.bell")!;
    btn.dispatchEvent(new Event("click", { bubbles: true }));
    await new Promise((r) => setTimeout(r, 10));
    await el.updateComplete;

    const empty = el.shadowRoot!.querySelector(".empty");
    expect(empty).not.toBeNull();
    expect(empty!.textContent).toContain("No notifications");
  });

  it("includes aria-label with unread count", async () => {
    mockFetch({ "unread-count": { unread: 3 } });
    const el = await mount();
    const btn = el.shadowRoot!.querySelector("button.bell");
    expect(btn!.getAttribute("aria-label")).toBe("Notifications (3 unread)");
  });

  it("closes panel on second click", async () => {
    mockFetch({
      "unread-count": { unread: 0 },
      "notifications?": [],
    });
    const el = await mount();

    const btn = el.shadowRoot!.querySelector("button.bell")!;
    btn.dispatchEvent(new Event("click", { bubbles: true }));
    await new Promise((r) => setTimeout(r, 10));
    await el.updateComplete;
    expect(el.shadowRoot!.querySelector(".panel")).not.toBeNull();

    btn.dispatchEvent(new Event("click", { bubbles: true }));
    await el.updateComplete;
    expect(el.shadowRoot!.querySelector(".panel")).toBeNull();
  });

  it("renders Mark read button for unread items", async () => {
    const items = [
      {
        id: 1,
        notification_type: "system_update",
        title: "Update",
        message: "System updated",
        is_read: 0,
        created_at: 1700000000,
      },
    ];
    mockFetch({
      "unread-count": { unread: 1 },
      "notifications?": items,
    });
    const el = await mount();

    const btn = el.shadowRoot!.querySelector("button.bell")!;
    btn.dispatchEvent(new Event("click", { bubbles: true }));
    await new Promise((r) => setTimeout(r, 10));
    await el.updateComplete;

    const markReadBtn = el.shadowRoot!.querySelector(".actions button");
    expect(markReadBtn).not.toBeNull();
    expect(markReadBtn!.textContent).toContain("Mark read");
  });

  it("renders Dismiss button for items", async () => {
    const items = [
      {
        id: 2,
        notification_type: "ytdlp_failure",
        title: "Download Error",
        message: "Failed",
        is_read: 1,
        created_at: 1700000000,
      },
    ];
    mockFetch({
      "unread-count": { unread: 0 },
      "notifications?": items,
    });
    const el = await mount();

    const btn = el.shadowRoot!.querySelector("button.bell")!;
    btn.dispatchEvent(new Event("click", { bubbles: true }));
    await new Promise((r) => setTimeout(r, 10));
    await el.updateComplete;

    const dismissBtn = el.shadowRoot!.querySelector('button[aria-label="Dismiss notification"]');
    expect(dismissBtn).not.toBeNull();
  });

  it("shows Mark all read button when there are unread items", async () => {
    const items = [
      {
        id: 1,
        notification_type: "system_update",
        title: "Update",
        message: "System updated",
        is_read: 0,
        created_at: 1700000000,
      },
    ];
    mockFetch({
      "unread-count": { unread: 1 },
      "notifications?": items,
    });
    const el = await mount();

    const btn = el.shadowRoot!.querySelector("button.bell")!;
    btn.dispatchEvent(new Event("click", { bubbles: true }));
    await new Promise((r) => setTimeout(r, 10));
    await el.updateComplete;

    const footer = el.shadowRoot!.querySelector(".footer button");
    expect(footer).not.toBeNull();
    expect(footer!.textContent).toContain("Mark all read");
  });

  it("handles mark read action", async () => {
    const items = [
      {
        id: 5,
        notification_type: "system_update",
        title: "N5",
        message: "msg",
        is_read: 0,
        created_at: 1700000000,
      },
    ];
    mockFetch({
      "unread-count": { unread: 1 },
      "notifications?": items,
    });
    const el = await mount();

    // Open panel
    const bellBtn = el.shadowRoot!.querySelector("button.bell")!;
    bellBtn.dispatchEvent(new Event("click", { bubbles: true }));
    await new Promise((r) => setTimeout(r, 10));
    await el.updateComplete;

    // Click "Mark read"
    const markBtn = el.shadowRoot!.querySelector(".actions button")!;
    (markBtn as HTMLElement).click();
    await new Promise((r) => setTimeout(r, 10));
    await el.updateComplete;

    // Should have called the PUT endpoint
    const putCalls = fetchSpy.mock.calls.filter((c: string[]) =>
      c[0].includes("/notifications/5/read"),
    );
    expect(putCalls.length).toBe(1);
  });

  it("handles dismiss action", async () => {
    const items = [
      {
        id: 7,
        notification_type: "ytdlp_failure",
        title: "N7",
        message: "msg",
        is_read: 1,
        created_at: 1700000000,
      },
    ];
    mockFetch({
      "unread-count": { unread: 0 },
      "notifications?": items,
    });
    const el = await mount();

    const bellBtn = el.shadowRoot!.querySelector("button.bell")!;
    bellBtn.dispatchEvent(new Event("click", { bubbles: true }));
    await new Promise((r) => setTimeout(r, 10));
    await el.updateComplete;

    const dismissBtn = el.shadowRoot!.querySelector(
      'button[aria-label="Dismiss notification"]',
    )! as HTMLElement;
    dismissBtn.click();
    await new Promise((r) => setTimeout(r, 10));
    await el.updateComplete;

    const deleteCalls = fetchSpy.mock.calls.filter(
      (c: unknown[]) =>
        (c[0] as string).includes("/notifications/7") &&
        (c[1] as RequestInit | undefined)?.method === "DELETE",
    );
    expect(deleteCalls.length).toBe(1);
  });

  it("handles mark all read action", async () => {
    const items = [
      {
        id: 1,
        notification_type: "system_update",
        title: "N1",
        message: "msg",
        is_read: 0,
        created_at: 1700000000,
      },
    ];
    mockFetch({
      "unread-count": { unread: 1 },
      "notifications?": items,
    });
    const el = await mount();

    const bellBtn = el.shadowRoot!.querySelector("button.bell")!;
    bellBtn.dispatchEvent(new Event("click", { bubbles: true }));
    await new Promise((r) => setTimeout(r, 10));
    await el.updateComplete;

    const markAllBtn = el.shadowRoot!.querySelector(".footer button")! as HTMLElement;
    markAllBtn.click();
    await new Promise((r) => setTimeout(r, 10));
    await el.updateComplete;

    const putCalls = fetchSpy.mock.calls.filter((c: string[]) =>
      c[0].includes("/notifications/read-all"),
    );
    expect(putCalls.length).toBe(1);
  });

  it("closes panel when clicking outside", async () => {
    mockFetch({
      "unread-count": { unread: 0 },
      "notifications?": [],
    });
    const el = await mount();

    const btn = el.shadowRoot!.querySelector("button.bell")!;
    btn.dispatchEvent(new Event("click", { bubbles: true }));
    await new Promise((r) => setTimeout(r, 10));
    await el.updateComplete;
    expect(el.shadowRoot!.querySelector(".panel")).not.toBeNull();

    // Click on body (outside the component)
    document.body.click();
    await el.updateComplete;
    expect(el.shadowRoot!.querySelector(".panel")).toBeNull();
  });

  it("renders timestamp for items", async () => {
    const items = [
      {
        id: 1,
        notification_type: "system_update",
        title: "T",
        message: "M",
        is_read: 0,
        created_at: 1700000000,
      },
    ];
    mockFetch({
      "unread-count": { unread: 1 },
      "notifications?": items,
    });
    const el = await mount();

    const btn = el.shadowRoot!.querySelector("button.bell")!;
    btn.dispatchEvent(new Event("click", { bubbles: true }));
    await new Promise((r) => setTimeout(r, 10));
    await el.updateComplete;

    const timestamp = el.shadowRoot!.querySelector(".timestamp");
    expect(timestamp).not.toBeNull();
    // Should contain some date string
    expect(timestamp!.textContent!.trim().length).toBeGreaterThan(0);
  });
});
