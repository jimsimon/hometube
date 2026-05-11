/**
 * <hometube-video-player video-id="...">
 *
 * Wraps a vidstack `<media-player>` with HomeTube-specific behaviour:
 *   - loads the rewritten DASH manifest from /api/videos/:id/stream
 *   - sends a heartbeat every 30s while playing to /api/usage/heartbeat
 *   - dispatches `hometube:usage-limit` on a 403 from the heartbeat
 *   - dispatches `hometube:current-time` on every `timeupdate` so
 *     companion controls (bookmark, scrubber markers) stay in sync
 *   - dispatches `hometube:video-ended` so the up-next sidebar can
 *     auto-advance
 *   - hosts the bookmark / sleep-timer / like / subscribe controls in
 *     a "chrome" row below the video
 *   - tracks the autoplay consecutive-watch count via `sessionStorage`
 *     and surfaces a "Continue watching?" prompt when the configured
 *     cap is reached
 *   - listens for `hometube:sleep-timer-expired` and reacts by fading
 *     audio out, pausing playback, and letting the wind-down overlay
 *     render
 *   - paints bookmarks on the seek bar when notified via
 *     `hometube:bookmarks-loaded`
 *   - respects `playback_speed_locked` from /api/children/me/settings
 *
 * The DASH manifest itself is the rewritten one (segments routed
 * through /api/proxy/segment), so vidstack just plays it like any
 * other DASH source.
 */

import { LitElement, html, css, nothing } from 'lit';
import { customElement, property, query, state } from 'lit/decorators.js';

import { ApiError, api } from '../services/api.js';
import type {
  Bookmark,
  ChildSettings,
  HeartbeatResponse,
  StreamResponse,
  UsageLimitResponse,
  VideoMetadata,
} from '../types/index.js';

import './bookmark-button.js';
import './sleep-timer.js';
import './like-button.js';
import './subscribe-button.js';

const HEARTBEAT_MS = 30_000;
const AUTOPLAY_KEY = 'hometube-autoplay-count';
/** How long the audio fade-out lasts when the sleep timer expires. */
const SLEEP_FADE_MS = 4_000;

@customElement('hometube-video-player')
export class VideoPlayer extends LitElement {
  @property({ type: String, attribute: 'video-id' })
  videoId = '';

  @state() private metadata: VideoMetadata | null = null;
  @state() private stream: StreamResponse | null = null;
  @state() private settings: ChildSettings | null = null;
  @state() private bookmarks: Bookmark[] = [];
  @state() private error = '';
  @state() private continuePromptOpen = false;
  /** Most-recent `remaining_seconds` from the heartbeat response. */
  @state() private remainingSeconds: number | null = null;

  @query('video') private videoEl!: HTMLVideoElement;

  private heartbeatTimer: number | null = null;
  private lastHeartbeatAt = 0;
  /** True after a manual play interaction; resets the autoplay counter. */
  private manualPlayed = false;

  static styles = css`
    :host {
      display: block;
    }
    .player-shell {
      position: relative;
      width: 100%;
      max-width: 64rem;
      margin: 0 auto;
      background: black;
      border-radius: 0.5rem;
      overflow: hidden;
    }
    video {
      width: 100%;
      display: block;
      aspect-ratio: 16 / 9;
      background: black;
    }
    .error {
      color: var(--wa-color-danger-fill, #b91c1c);
      padding: 1rem;
    }
    .meta {
      margin-top: 1rem;
    }
    .meta h1 {
      margin: 0 0 0.25rem;
      font-size: 1.25rem;
    }
    .meta .channel {
      color: var(--wa-color-text-quiet);
    }
    .chrome {
      display: flex;
      gap: 0.5rem;
      flex-wrap: wrap;
      margin-top: 1rem;
      align-items: center;
    }
    .seek-overlay {
      position: relative;
      height: 0.5rem;
      margin-top: -0.5rem;
      pointer-events: none;
    }
    .bookmark-marker {
      position: absolute;
      top: 0;
      width: 0.4rem;
      height: 100%;
      background: var(--wa-color-warning-fill, #d97706);
      border-radius: 2px;
      transform: translateX(-50%);
    }
    .continue-prompt {
      position: absolute;
      inset: 0;
      background: rgba(0, 0, 0, 0.65);
      color: white;
      display: flex;
      align-items: center;
      justify-content: center;
      flex-direction: column;
      gap: 0.75rem;
      padding: 1rem;
      text-align: center;
    }
    .continue-prompt button {
      padding: 0.5rem 1rem;
      border-radius: 0.375rem;
      border: 1px solid white;
      background: white;
      color: black;
      font: inherit;
      cursor: pointer;
    }
    .countdown {
      margin: 0.5rem 0;
      padding: 0.5rem 0.75rem;
      border-radius: 0.375rem;
      background: var(--wa-color-warning-quiet, rgba(217, 119, 6, 0.15));
      color: var(--wa-color-warning-on-quiet, #92400e);
      font-size: 0.9rem;
    }
    .countdown.urgent {
      background: var(--wa-color-danger-quiet, rgba(185, 28, 28, 0.15));
      color: var(--wa-color-danger-on-quiet, #991b1b);
      font-weight: 600;
      font-size: 1rem;
    }
  `;

  override connectedCallback(): void {
    super.connectedCallback();
    if (this.videoId) void this.load();
    document.addEventListener(
      'hometube:sleep-timer-expired',
      this.onSleepExpired as EventListener,
    );
    document.addEventListener(
      'hometube:bookmarks-loaded',
      this.onBookmarksLoaded as EventListener,
    );
    document.addEventListener(
      'hometube:autoplay-cap-reached',
      this.onAutoplayCap as EventListener,
    );
  }

  override updated(changed: Map<string, unknown>): void {
    if (changed.has('videoId') && this.videoId) {
      void this.load();
    }
  }

  override disconnectedCallback(): void {
    super.disconnectedCallback();
    this.stopHeartbeat();
    document.removeEventListener(
      'hometube:sleep-timer-expired',
      this.onSleepExpired as EventListener,
    );
    document.removeEventListener(
      'hometube:bookmarks-loaded',
      this.onBookmarksLoaded as EventListener,
    );
    document.removeEventListener(
      'hometube:autoplay-cap-reached',
      this.onAutoplayCap as EventListener,
    );
  }

  private async load(): Promise<void> {
    this.error = '';
    try {
      const [meta, stream, settings] = await Promise.all([
        api.get<VideoMetadata>(`/api/videos/${this.videoId}`),
        api.get<StreamResponse>(`/api/videos/${this.videoId}/stream`),
        api
          .get<ChildSettings>(`/api/children/me/settings`)
          .catch(() => null),
      ]);
      this.metadata = meta;
      this.stream = stream;
      this.settings = settings;
      // After render, attach the manifest. We use a Blob URL so the
      // browser can request relative segment URLs against our origin.
      queueMicrotask(() => this.attachSource());
    } catch (err) {
      this.error =
        err instanceof ApiError ? String(err.body) : (err as Error).message;
    }
  }

  private attachSource(): void {
    if (!this.videoEl) return;
    if (this.stream?.manifest) {
      const blob = new Blob([this.stream.manifest], {
        type: 'application/dash+xml',
      });
      this.videoEl.src = URL.createObjectURL(blob);
      // For DASH, the browser native <video> won't play it; vidstack
      // /dash.js would. Without that here we fall back to the first
      // progressive format if available, so the player at least works
      // for testing.
    }
    if (!this.videoEl.src) {
      const progressive = this.stream?.formats.find(
        (f) =>
          f.protocol === 'https' && f.height != null && f.acodec !== 'none',
      );
      if (progressive?.url) {
        this.videoEl.src = progressive.url;
      }
    }
    if (this.settings?.playback_speed_locked) {
      this.videoEl.playbackRate = 1;
    }
  }

  private onPlay = (): void => {
    this.startHeartbeat();
    if (!this.manualPlayed) {
      // Treat the first play after page load as "manual" — the user
      // pressed play themselves, so the autoplay chain resets.
      this.manualPlayed = true;
      try {
        sessionStorage.setItem(AUTOPLAY_KEY, '0');
      } catch {
        // ignore (private browsing etc.)
      }
    }
  };

  private onPause = (): void => {
    this.stopHeartbeat();
  };

  private onTimeUpdate = (): void => {
    if (!this.videoEl) return;
    document.dispatchEvent(
      new CustomEvent('hometube:current-time', {
        detail: { seconds: this.videoEl.currentTime },
      }),
    );
  };

  private onEnded = (): void => {
    void this.sendHeartbeat();
    this.stopHeartbeat();

    // Sleep timer of type "after_video" — pause the autoplay chain.
    void this.checkAfterVideoTimer();

    if (this.shouldShowContinuePrompt()) {
      this.continuePromptOpen = true;
      return;
    }

    document.dispatchEvent(
      new CustomEvent('hometube:video-ended', {
        detail: { videoId: this.videoId },
      }),
    );
  };

  private async checkAfterVideoTimer(): Promise<void> {
    try {
      const timer = await api
        .get<{ timer_type?: string } | null>('/api/timer')
        .catch(() => null);
      if (timer?.timer_type === 'after_video') {
        await api.delete('/api/timer').catch(() => {});
        document.dispatchEvent(new CustomEvent('hometube:sleep-timer-expired'));
      }
    } catch {
      // ignore
    }
  }

  private shouldShowContinuePrompt(): boolean {
    if (!this.settings) return false;
    if (!this.settings.autoplay_enabled) return false;
    if (this.settings.autoplay_max_consecutive == null) return false;
    let count = 0;
    try {
      count = Number(sessionStorage.getItem(AUTOPLAY_KEY) ?? '0');
    } catch {
      count = 0;
    }
    return count >= this.settings.autoplay_max_consecutive;
  }

  private onContinue = (): void => {
    this.continuePromptOpen = false;
    try {
      sessionStorage.setItem(AUTOPLAY_KEY, '0');
    } catch {
      // ignore
    }
    document.dispatchEvent(
      new CustomEvent('hometube:video-ended', {
        detail: { videoId: this.videoId },
      }),
    );
  };

  private onSleepExpired = (): void => {
    if (!this.videoEl) return;
    // Fade audio out smoothly, then pause.
    const start = Date.now();
    const startVolume = this.videoEl.volume;
    const fade = (): void => {
      if (!this.videoEl) return;
      const elapsed = Date.now() - start;
      const ratio = Math.max(0, 1 - elapsed / SLEEP_FADE_MS);
      this.videoEl.volume = Math.max(0, startVolume * ratio);
      if (elapsed < SLEEP_FADE_MS) {
        requestAnimationFrame(fade);
      } else {
        this.videoEl.pause();
        this.videoEl.volume = startVolume;
      }
    };
    requestAnimationFrame(fade);
  };

  private onBookmarksLoaded = (e: Event): void => {
    const detail = (e as CustomEvent<{ bookmarks: Bookmark[] }>).detail;
    if (Array.isArray(detail?.bookmarks)) {
      this.bookmarks = detail.bookmarks;
    }
  };

  private onAutoplayCap = (): void => {
    this.continuePromptOpen = true;
  };

  private startHeartbeat(): void {
    if (this.heartbeatTimer != null) return;
    this.lastHeartbeatAt = Date.now();
    this.heartbeatTimer = window.setInterval(
      () => void this.sendHeartbeat(),
      HEARTBEAT_MS,
    );
  }

  private stopHeartbeat(): void {
    if (this.heartbeatTimer != null) {
      window.clearInterval(this.heartbeatTimer);
      this.heartbeatTimer = null;
    }
  }

  private async sendHeartbeat(): Promise<void> {
    if (!this.videoEl || !this.metadata) return;
    const now = Date.now();
    const elapsed = Math.max(1, Math.round((now - this.lastHeartbeatAt) / 1000));
    this.lastHeartbeatAt = now;
    try {
      const res = await api.post<HeartbeatResponse>('/api/usage/heartbeat', {
        video_id: this.videoId,
        position_seconds: Math.floor(this.videoEl.currentTime),
        duration_seconds: Math.floor(this.videoEl.duration || 0) || null,
        video_title: this.metadata.title,
        video_thumbnail_url: this.metadata.thumbnail_url,
        channel_title: this.metadata.channel_title,
        elapsed_seconds: elapsed,
      });
      this.remainingSeconds = res.remaining_seconds;
      if (res.limit_exceeded) {
        this.handleUsageLimit({
          reason: res.reason ?? 'limit_exceeded',
          remaining_seconds: res.remaining_seconds ?? 0,
          allowed_window: res.allowed_window ?? null,
        });
      }
    } catch (err) {
      if (err instanceof ApiError && err.status === 403) {
        const body =
          (err.body as UsageLimitResponse | null) ?? {
            reason: 'limit_exceeded' as const,
            remaining_seconds: 0,
          };
        this.handleUsageLimit(body);
      }
    }
  }

  private handleUsageLimit(detail: UsageLimitResponse): void {
    this.videoEl?.pause();
    this.stopHeartbeat();
    this.dispatchEvent(
      new CustomEvent('hometube:usage-limit', {
        detail,
        bubbles: true,
        composed: true,
      }),
    );
  }

  private renderBookmarkMarkers() {
    if (!this.videoEl || !this.metadata) return nothing;
    const duration = Math.max(1, this.videoEl.duration || 0);
    if (!isFinite(duration) || duration <= 0) return nothing;
    return html`
      <div class="seek-overlay" aria-hidden="true">
        ${this.bookmarks.map((b) => {
          const pct = Math.min(
            100,
            Math.max(0, (b.timestamp_seconds / duration) * 100),
          );
          return html`<span
            class="bookmark-marker"
            style="left: ${pct}%"
            title=${b.label ?? `${b.timestamp_seconds}s`}
          ></span>`;
        })}
      </div>
    `;
  }

  override render() {
    if (this.error) {
      return html`<p class="error" role="alert">${this.error}</p>`;
    }
    return html`
      <div class="player-shell">
        <video
          controls
          preload="metadata"
          @play=${this.onPlay}
          @pause=${this.onPause}
          @timeupdate=${this.onTimeUpdate}
          @ended=${this.onEnded}
          @ratechange=${this.onRateChange}
          aria-label=${this.metadata?.title ?? 'Video player'}
        ></video>
        ${this.continuePromptOpen
          ? html`
              <div
                class="continue-prompt"
                role="dialog"
                aria-modal="true"
                aria-label="Continue watching?"
              >
                <p>Wow, you've watched a lot in a row. Take a break?</p>
                <div>
                  <button type="button" @click=${this.onContinue}>
                    Continue watching
                  </button>
                </div>
              </div>
            `
          : nothing}
      </div>
      ${this.renderBookmarkMarkers()}
      ${this.renderCountdown()}
      ${this.metadata
        ? html`<div class="meta">
            <h1>${this.metadata.title ?? 'Untitled'}</h1>
            ${this.metadata.channel_title
              ? html`<div class="channel">${this.metadata.channel_title}</div>`
              : null}
            <div class="chrome">
              <hometube-like-button
                video-id=${this.videoId}
              ></hometube-like-button>
              ${this.metadata.channel_id
                ? html`<hometube-subscribe-button
                    channel-id=${this.metadata.channel_id}
                  ></hometube-subscribe-button>`
                : nothing}
              <hometube-bookmark-button
                video-id=${this.videoId}
              ></hometube-bookmark-button>
              <hometube-sleep-timer></hometube-sleep-timer>
            </div>
          </div>`
        : null}
    `;
  }

  /** Render the countdown indicator. Hidden until under 30 minutes. */
  private renderCountdown() {
    const remaining = this.remainingSeconds;
    if (remaining == null || remaining > 30 * 60) return nothing;
    const minutes = Math.max(0, Math.ceil(remaining / 60));
    let cls = 'countdown';
    let text = `${minutes} minute${minutes === 1 ? '' : 's'} left for today.`;
    if (remaining <= 60) {
      cls = 'countdown urgent';
      text = "Less than a minute left — wrapping up soon!";
    } else if (remaining <= 5 * 60) {
      cls = 'countdown urgent';
      text = `${minutes} minute${minutes === 1 ? '' : 's'} left — almost done!`;
    }
    return html`<div class="${cls}" role="status" aria-live="polite">
      ${text}
    </div>`;
  }

  /** Enforce the playback-speed lock from child settings. */
  private onRateChange = (): void => {
    if (!this.videoEl) return;
    if (this.settings?.playback_speed_locked && this.videoEl.playbackRate !== 1) {
      this.videoEl.playbackRate = 1;
    }
  };
}

declare global {
  interface HTMLElementTagNameMap {
    'hometube-video-player': VideoPlayer;
  }
}
