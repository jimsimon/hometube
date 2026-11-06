import { describe, expect, it } from "vitest";

import { ProgressGate } from "./progress-gate.js";

describe("ProgressGate", () => {
  it("starts with nothing to send", () => {
    const gate = new ProgressGate();
    expect(gate.hasUnsentProgress()).toBe(false);
  });

  it("reports unsent progress once the playhead moves", () => {
    const gate = new ProgressGate();
    gate.markDirty();
    expect(gate.hasUnsentProgress()).toBe(true);
  });

  it("clears after a write is sent and stays clear while idle", () => {
    const gate = new ProgressGate();
    gate.markDirty();
    gate.markSent();
    expect(gate.hasUnsentProgress()).toBe(false);
    // A paused tab that never moves the playhead again must not
    // re-send its old position on every hide/unload.
    expect(gate.hasUnsentProgress()).toBe(false);
  });

  it("becomes dirty again after new movement following a send", () => {
    const gate = new ProgressGate();
    gate.markDirty();
    gate.markSent();
    gate.markDirty();
    expect(gate.hasUnsentProgress()).toBe(true);
  });

  it("reset drops pending progress when the video changes", () => {
    const gate = new ProgressGate();
    gate.markDirty();
    gate.reset();
    expect(gate.hasUnsentProgress()).toBe(false);
  });
});
