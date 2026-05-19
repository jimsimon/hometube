import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import { debounce } from "./debounce.js";

describe("debounce", () => {
  beforeEach(() => {
    vi.useFakeTimers();
  });

  afterEach(() => {
    vi.useRealTimers();
  });

  it("delays invocation until wait has elapsed since the last call", () => {
    const fn = vi.fn();
    const d = debounce(fn, 100);

    d("a");
    d("b");
    d("c");
    expect(fn).not.toHaveBeenCalled();

    vi.advanceTimersByTime(99);
    expect(fn).not.toHaveBeenCalled();

    vi.advanceTimersByTime(1);
    expect(fn).toHaveBeenCalledTimes(1);
    expect(fn).toHaveBeenCalledWith("c");
  });

  it("each call resets the wait window", () => {
    const fn = vi.fn();
    const d = debounce(fn, 100);

    d("a");
    vi.advanceTimersByTime(80);
    d("b");
    vi.advanceTimersByTime(80);
    // Still pending — second call reset the timer.
    expect(fn).not.toHaveBeenCalled();

    vi.advanceTimersByTime(20);
    expect(fn).toHaveBeenCalledTimes(1);
    expect(fn).toHaveBeenCalledWith("b");
  });

  it("cancel() drops the pending invocation", () => {
    const fn = vi.fn();
    const d = debounce(fn, 100);

    d("a");
    d.cancel();
    vi.advanceTimersByTime(500);
    expect(fn).not.toHaveBeenCalled();
  });

  it("flush() fires immediately with the latest args", () => {
    const fn = vi.fn();
    const d = debounce(fn, 100);

    d("a");
    d("b");
    d.flush();

    expect(fn).toHaveBeenCalledTimes(1);
    expect(fn).toHaveBeenCalledWith("b");

    // No additional invocation after the original timeout would have fired.
    vi.advanceTimersByTime(500);
    expect(fn).toHaveBeenCalledTimes(1);
  });

  it("flush() is a no-op when nothing is pending", () => {
    const fn = vi.fn();
    const d = debounce(fn, 100);

    d.flush();
    expect(fn).not.toHaveBeenCalled();
  });
});
