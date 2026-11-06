/**
 * Tracks whether the player has produced playback progress that the
 * server hasn't seen yet.
 *
 * Why this exists: `watch_history.progress_seconds` is the resume
 * point for a video, and *every* client write replaces it. The player
 * writes on pause, seek, the 30s heartbeat, `ended`, and — via
 * `navigator.sendBeacon` — whenever the page is hidden or unloaded.
 * That last path fires for a tab that has been sitting paused in the
 * background for days (laptop lid closed and reopened, a forgotten
 * Chromebook tab, a second device). Such a tab still knows only the
 * position it was paused at long ago, so each hide/unload re-sent that
 * stale position and clobbered whatever the child had watched up to on
 * another device since. The symptom was a resume point that kept
 * snapping back to the same old timestamp across days.
 *
 * The gate records when the playhead actually moved (timeupdate while
 * playing, a user seek, the natural end of the video) and is cleared
 * once a write has been accepted. Position-only writers (pause / seek
 * progress POSTs and the hide/unload beacon) consult it and skip the
 * request when there is nothing new to say — the server already holds
 * exactly this position.
 *
 * The regular 30s heartbeat is intentionally not gated: it only runs
 * while the video is playing, and it is also the screen-time ledger,
 * so a buffering stall should still be accounted for.
 */
export class ProgressGate {
  private dirty = false;

  /**
   * Record that the playhead moved. Call from `timeupdate` (when the
   * time actually changed), `seeked`, and `ended`.
   */
  markDirty(): void {
    this.dirty = true;
  }

  /** True when a position write would carry information the server lacks. */
  hasUnsentProgress(): boolean {
    return this.dirty;
  }

  /** Record that a write carrying the current position was accepted/queued. */
  markSent(): void {
    this.dirty = false;
  }

  /** Forget everything — call when the player switches to a new video. */
  reset(): void {
    this.dirty = false;
  }
}
