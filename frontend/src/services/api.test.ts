/**
 * Unit tests for the API fetch wrapper.
 */
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import { ApiError, api } from "./api.js";

interface FetchStub {
  status: number;
  body?: unknown;
  contentType?: string;
}

function stubFetch(stub: FetchStub | FetchStub[]): ReturnType<typeof vi.spyOn> {
  const queue = Array.isArray(stub) ? [...stub] : [stub];
  return vi.spyOn(globalThis, "fetch").mockImplementation(async (_input) => {
    const next = queue.shift() ?? queue[queue.length - 1];
    if (!next) throw new Error("no fetch stub configured");
    const ct = next.contentType ?? "application/json";
    const bodyText = ct.includes("json")
      ? JSON.stringify(next.body ?? null)
      : String(next.body ?? "");
    return new Response(bodyText, {
      status: next.status,
      headers: { "content-type": ct },
    });
  });
}

describe("api service", () => {
  let fetchSpy: ReturnType<typeof vi.spyOn>;

  beforeEach(() => {
    // no-op default — each test sets its own
  });

  afterEach(() => {
    fetchSpy?.mockRestore();
    vi.restoreAllMocks();
  });

  it("parses JSON GET responses", async () => {
    fetchSpy = stubFetch({ status: 200, body: { ok: true } });
    const out = await api.get<{ ok: boolean }>("/api/x");
    expect(out).toEqual({ ok: true });
    const call = fetchSpy.mock.calls[0]!;
    expect(call[0]).toBe("/api/x");
    expect((call[1] as RequestInit).method).toBe("GET");
  });

  it("serialises JSON bodies on POST and sets Content-Type", async () => {
    fetchSpy = stubFetch({ status: 200, body: {} });
    await api.post("/api/x", { hello: "world" });
    const init = fetchSpy.mock.calls[0]![1] as RequestInit;
    expect(init.method).toBe("POST");
    expect(init.body).toBe(JSON.stringify({ hello: "world" }));
    const headers = init.headers as Record<string, string>;
    expect(headers["Content-Type"]).toBe("application/json");
    expect(headers.Accept).toBe("application/json");
  });

  it("issues PUT and DELETE with the right method", async () => {
    fetchSpy = stubFetch([
      { status: 200, body: { ok: 1 } },
      { status: 200, body: { ok: 1 } },
    ]);
    await api.put("/api/x/1", { v: 1 });
    await api.delete("/api/x/1");
    expect((fetchSpy.mock.calls[0]![1] as RequestInit).method).toBe("PUT");
    expect((fetchSpy.mock.calls[1]![1] as RequestInit).method).toBe("DELETE");
  });

  it("falls back to text when the response is not JSON", async () => {
    fetchSpy = stubFetch({ status: 200, contentType: "text/plain", body: "hi" });
    const out = await api.get<string>("/api/x");
    expect(out).toBe("hi");
  });

  it("throws ApiError with status + body on non-2xx", async () => {
    fetchSpy = stubFetch({ status: 403, body: { reason: "forbidden" } });
    await expect(api.get("/api/x")).rejects.toMatchObject({
      name: "ApiError",
      status: 403,
      body: { reason: "forbidden" },
    });
  });

  it("ApiError exposes a useful message", () => {
    const err = new ApiError(500, { detail: "boom" }, "HTTP 500 on /api/x");
    expect(err.message).toContain("500");
    expect(err.body).toEqual({ detail: "boom" });
    expect(err.status).toBe(500);
  });
});
