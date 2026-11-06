/**
 * Touch double-tap detection for the video surface.
 *
 * Pure logic, no DOM: the player feeds it one `TapPoint` per completed
 * tap and gets back whether that tap completed a double tap and which
 * horizontal zone of the surface it landed in. The player maps zones to
 * actions (left → rewind, right → fast-forward, center → fullscreen).
 *
 * Why not rely on shaka-player's built-in `seekOnTaps`? Its hidden
 * seek buttons only intercept taps while the control bar is visible.
 * With the controls faded out (the normal state while a video plays
 * fullscreen) the first tap merely reveals them and the second tap
 * reaches the container's own double-tap handler, which toggles
 * fullscreen. So double-tapping an edge mid-playback minimised the
 * video instead of skipping 10 s. This tracker is consulted in the
 * capture phase, before any of shaka's listeners, so it sees every tap
 * regardless of control visibility.
 */

export type TapZone = "left" | "center" | "right";

export interface TapPoint {
  /** Horizontal position relative to the surface's left edge, in px. */
  x: number;
  /** Vertical position relative to the surface's top edge, in px. */
  y: number;
  /** Timestamp in ms (e.g. `Date.now()` or `event.timeStamp`). */
  time: number;
}

export type TapResult = { kind: "single" } | { kind: "double"; zone: TapZone };

/** Two taps this close in time form a double tap. */
export const DOUBLE_TAP_WINDOW_MS = 350;
/** …and must land this close to each other. */
export const DOUBLE_TAP_SLOP_PX = 48;
/** Fraction of the surface width on each side that counts as an edge. */
export const EDGE_ZONE_FRACTION = 0.35;
/** Seconds skipped per edge double tap. */
export const TAP_SEEK_SECONDS = 10;

/** Classify a horizontal position into left / center / right. */
export function tapZone(x: number, width: number): TapZone {
  if (!(width > 0)) return "center";
  const fraction = x / width;
  if (fraction <= EDGE_ZONE_FRACTION) return "left";
  if (fraction >= 1 - EDGE_ZONE_FRACTION) return "right";
  return "center";
}

export class TapTracker {
  private last: TapPoint | null = null;

  /**
   * Register a completed tap. Returns `double` when it follows another
   * tap within the time window and slop radius. The tap is always
   * remembered, so a third quick tap in the same spot is reported as a
   * double again — this is what lets a child tap-tap-tap an edge to
   * skip 30 s, matching YouTube's own behaviour.
   */
  register(tap: TapPoint, surfaceWidth: number): TapResult {
    const prev = this.last;
    this.last = tap;
    if (
      prev &&
      tap.time - prev.time >= 0 &&
      tap.time - prev.time <= DOUBLE_TAP_WINDOW_MS &&
      Math.hypot(tap.x - prev.x, tap.y - prev.y) <= DOUBLE_TAP_SLOP_PX
    ) {
      return { kind: "double", zone: tapZone(tap.x, surfaceWidth) };
    }
    return { kind: "single" };
  }

  /** Forget the pending tap (e.g. the finger moved — it was a swipe). */
  reset(): void {
    this.last = null;
  }
}

/**
 * Clamp a seek target into the playable range so a rewind near the
 * start lands on `start` and a skip near the end lands on `end`.
 */
export function clampSeekTarget(
  current: number,
  deltaSeconds: number,
  range: { start: number; end: number },
): number {
  const target = current + deltaSeconds;
  const lo = Number.isFinite(range.start) ? range.start : 0;
  const hi = Number.isFinite(range.end) ? range.end : Number.POSITIVE_INFINITY;
  return Math.min(hi, Math.max(lo, target));
}
