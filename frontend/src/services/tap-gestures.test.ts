import { describe, expect, it } from "vitest";

import {
  DOUBLE_TAP_SLOP_PX,
  DOUBLE_TAP_WINDOW_MS,
  TapTracker,
  clampSeekTarget,
  tapZone,
} from "./tap-gestures.js";

describe("tapZone", () => {
  it("splits the surface into left / center / right", () => {
    expect(tapZone(10, 1000)).toBe("left");
    expect(tapZone(350, 1000)).toBe("left");
    expect(tapZone(500, 1000)).toBe("center");
    expect(tapZone(651, 1000)).toBe("right");
    expect(tapZone(990, 1000)).toBe("right");
  });

  it("treats an unknown width as center so nothing seeks by accident", () => {
    expect(tapZone(10, 0)).toBe("center");
    expect(tapZone(10, Number.NaN)).toBe("center");
  });
});

describe("TapTracker", () => {
  it("reports the first tap as single", () => {
    const t = new TapTracker();
    expect(t.register({ x: 10, y: 10, time: 1000 }, 1000)).toEqual({ kind: "single" });
  });

  it("reports a quick second tap in the same spot as a double with its zone", () => {
    const t = new TapTracker();
    t.register({ x: 900, y: 200, time: 1000 }, 1000);
    expect(t.register({ x: 905, y: 204, time: 1000 + 200 }, 1000)).toEqual({
      kind: "double",
      zone: "right",
    });
  });

  it("keeps reporting doubles on a run of quick taps", () => {
    const t = new TapTracker();
    t.register({ x: 50, y: 50, time: 0 }, 1000);
    expect(t.register({ x: 50, y: 50, time: 200 }, 1000).kind).toBe("double");
    expect(t.register({ x: 50, y: 50, time: 400 }, 1000).kind).toBe("double");
    expect(t.register({ x: 50, y: 50, time: 600 }, 1000).kind).toBe("double");
  });

  it("does not pair taps that are too far apart in time", () => {
    const t = new TapTracker();
    t.register({ x: 50, y: 50, time: 0 }, 1000);
    expect(t.register({ x: 50, y: 50, time: DOUBLE_TAP_WINDOW_MS + 1 }, 1000)).toEqual({
      kind: "single",
    });
  });

  it("does not pair taps that are too far apart on screen", () => {
    const t = new TapTracker();
    t.register({ x: 50, y: 50, time: 0 }, 1000);
    expect(t.register({ x: 50 + DOUBLE_TAP_SLOP_PX + 1, y: 50, time: 100 }, 1000)).toEqual({
      kind: "single",
    });
  });

  it("reset drops the pending tap", () => {
    const t = new TapTracker();
    t.register({ x: 50, y: 50, time: 0 }, 1000);
    t.reset();
    expect(t.register({ x: 50, y: 50, time: 100 }, 1000)).toEqual({ kind: "single" });
  });
});

describe("clampSeekTarget", () => {
  it("adds the delta inside the range", () => {
    expect(clampSeekTarget(60, 10, { start: 0, end: 300 })).toBe(70);
    expect(clampSeekTarget(60, -10, { start: 0, end: 300 })).toBe(50);
  });

  it("clamps to the range edges", () => {
    expect(clampSeekTarget(4, -10, { start: 0, end: 300 })).toBe(0);
    expect(clampSeekTarget(295, 10, { start: 0, end: 300 })).toBe(300);
  });

  it("tolerates a non-finite range", () => {
    expect(clampSeekTarget(5, -10, { start: Number.NaN, end: Number.NaN })).toBe(0);
    expect(clampSeekTarget(5, 10, { start: 0, end: Number.POSITIVE_INFINITY })).toBe(15);
  });
});
