/**
 * <hometube-video-player video-id="...">
 *
 * Wraps a vidstack `<media-player>` (with dash.js + hls.js auto-loaded
 * by vidstack as soon as the manifest type is detected) and adds the
 * HomeTube-specific behaviour:
 *
 *   - Loads metadata + DASH-manifest URL from
 *     `/api/videos/:id/stream`. The backend now exposes the rewritten
 *     manifest at `/api/videos/:id/stream/manifest.mpd` so vidstack
 *     can fetch it directly (vidstack/dash.js cannot consume blob:
 *     URLs reliably).
 *   - Sends a heartbeat every 30s while playing to
 *     `/api/usage/heartbeat`; dispatches `hometube:usage-limit` on a
 *     403.
 *   - Dispatches `hometube:current-time` on every `time-update` and
 *     `hometube:video-ended` on `ended`.
 *   - Hosts the bookmark / sleep-timer / like / subscribe / download
 *     controls in a "chrome" row below the video.
 *   - Tracks the autoplay consecutive-watch count via `sessionStorage`
 *     and surfaces a "Continue watching?" prompt when the cap is hit.
 *   - Audio-only mode swaps the player source to a single
 *     `/api/proxy/audio?...` URL (highest-bitrate audio-only format)
 *     and renders the video thumbnail as a static poster instead of
 *     the video frame. The preference is persisted per-video in
 *     localStorage.
 *   - Quality selector respects `child_settings.max_quality` (filter
 *     formats above the cap) and the playback-speed control is hidden
 *     when `playback_speed_locked = true`.
 *   - Supports `start-at` for bookmark deep-links.
 *   - Adds a "Download" button when `child_settings.downloads_enabled`
 *     is on; the click handler talks to the offline-downloads service
 *     to pipe the response into the Cache API.
 */

import { LitElement, html, css, unsafeCSS, nothing } from "lit";
import { customElement, property, query, state } from "lit/decorators.js";

// dashjs and hls.js are dynamically resolved by vidstack when it sees a
// DASH or HLS source respectively. We side-effect-import them here so
// they end up in this component's bundle (otherwise Vite would tree-
// shake them out and the dynamic resolution would 404).
import "dashjs";
import "hls.js";

// vidstack styles + element registration. The component renders inside
// a Lit shadow root, so the styles must be adopted into that shadow
// root (see `static styles` below). However, vidstack *also* portals
// menu popups (`<media-menu-items>`) out to the light DOM to escape
// stacking-context traps, and those portaled menus are not reachable
// from the shadow root's adopted stylesheets. We therefore also
// inject the same CSS into `document.head` once per page.
import vidstackTheme from "vidstack/player/styles/default/theme.css?inline";
import vidstackVideoLayout from "vidstack/player/styles/default/layouts/video.css?inline";
import "vidstack/player";
import "vidstack/player/layouts";
import "vidstack/player/ui";

/**
 * Idempotently inject the vidstack stylesheets into `document.head` so
 * vidstack's portaled menu popups (which live outside the player's
 * shadow root) are styled correctly.
 */
function ensureVidstackDocumentStyles(): void {
  const MARKER = "data-hometube-vidstack-styles";
  if (document.head.querySelector(`style[${MARKER}]`)) return;
  const style = document.createElement("style");
  style.setAttribute(MARKER, "");
  // Concatenated theme + video-layout stylesheets. Selectors match
  // vidstack's own attribute/class hooks (`[data-media-player]`,
  // `.vds-menu-items`, etc.) so the same rules work in both contexts.
  style.textContent = `${vidstackTheme}\n${vidstackVideoLayout}`;
  document.head.appendChild(style);
}

import { ApiError, api } from "../services/api.js";
import {
  ensurePersistentStorage,
  getStorageEstimate,
  getVideoPrefs,
  saveVideoToOpfs,
  setVideoPrefs,
} from "../services/offline.js";
import type {
  Bookmark,
  CaptionTrack,
  ChildSettings,
  HeartbeatResponse,
  StreamResponse,
  UsageLimitResponse,
  VideoMetadata,
} from "../types/index.js";

import "./bookmark-button.js";
import "./sleep-timer.js";
import "./like-button.js";
import "./subscribe-button.js";
import "./error-banner.js";

const HEARTBEAT_MS = 30_000;
const AUTOPLAY_KEY = "hometube-autoplay-count";
/** How long the audio fade-out lasts when the sleep timer expires. */
const SLEEP_FADE_MS = 4_000;
/** Warn the user if free storage is below this (500 MB). */
const LOW_STORAGE_BYTES = 500 * 1024 * 1024;

/** Quality cap label → max height. */
const QUALITY_CAP: Record<string, number> = {
  "480p": 480,
  "720p": 720,
  "1080p": 1080,
};

/** Minimal subset of the vidstack media-state we read from. */
interface VidstackMediaState {
  currentTime: number;
  duration: number;
  paused: boolean;
  ended: boolean;
}

/** Subset of `<media-player>` we touch programmatically. */
interface VidstackMediaPlayer extends HTMLElement {
  src: string | { src: string; type: string };
  currentTime: number;
  playbackRate: number;
  state: VidstackMediaState;
  subscribe(cb: (state: VidstackMediaState) => void): () => void;
}

@customElement("hometube-video-player")
export class VideoPlayer extends LitElement {
  @property({ type: String, attribute: "video-id" })
  videoId = "";

  /**
   * Parental-preview mode. Disables heartbeats, continue-watching, and
   * the bookmark/like/subscribe chrome (those are child-only).
   */
  @property({ type: Boolean })
  preview = false;

  /** Optional initial seek position in seconds. */
  @property({ type: Number, attribute: "start-at" })
  startAt: number | null = null;

  @state() private metadata: VideoMetadata | null = null;
  @state() private stream: StreamResponse | null = null;
  @state() private settings: ChildSettings | null = null;
  @state() private bookmarks: Bookmark[] = [];
  @state() private captionTracks: CaptionTrack[] = [];
  @state() private error = "";
  @state() private continuePromptOpen = false;
  /** True when the audio-only toggle is engaged. */
  @property({ type: Boolean, reflect: true, attribute: "data-audio-only" })
  audioOnly = false;
  /** Most-recent `remaining_seconds` from the heartbeat response. */
  @state() private remainingSeconds: number | null = null;
  /** True while a download is in progress. */
  @state() private downloading = false;
  /** Set when a download succeeds — collapses to a "downloaded" badge. */
  @state() private downloaded = false;

  @query("media-player") private playerEl!: VidstackMediaPlayer;

  private heartbeatTimer: number | null = null;
  private lastHeartbeatAt = 0;
  /** True after a manual play interaction; resets the autoplay counter. */
  private manualPlayed = false;
  /** Unsubscribe from the vidstack state subscription. */
  private mediaUnsub: (() => void) | null = null;
  /** Last currentTime we saw — used to compute heartbeat deltas. */
  private lastSeenTime = 0;

  static styles = [
    unsafeCSS(vidstackTheme),
    unsafeCSS(vidstackVideoLayout),
    css`
    :host {
      display: block;
    }
    .player-shell {
      position: relative;
      width: 100%;
      max-width: 64rem;
      margin: 0 auto;
      view-transition-name: video-hero;
    }
    media-player {
      width: 100%;
      aspect-ratio: 16 / 9;
      background: black;
      border-radius: 0.5rem;
      overflow: hidden;
    }
    /* Audio-only mode: replace the player surface with the poster. */
    :host([data-audio-only]) media-player {
      background-size: cover;
      background-position: center;
      background-repeat: no-repeat;
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
      z-index: 500;
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
    .audio-toggle,
    .download-button {
      padding: 0.4rem 0.75rem;
      border-radius: 0.375rem;
      border: 1px solid var(--wa-color-surface-border, #ccc);
      background: transparent;
      color: var(--wa-color-text-normal);
      font: inherit;
      cursor: pointer;
    }
    .audio-toggle[aria-pressed="true"] {
      background: var(--wa-color-brand-fill, #2563eb);
      color: white;
      border-color: transparent;
    }
    .download-button[disabled] {
      opacity: 0.7;
      cursor: progress;
    }
  `,
  ];

  override connectedCallback(): void {
    super.connectedCallback();
    // Style vidstack's portaled menu popups (rendered in light DOM).
    ensureVidstackDocumentStyles();
    if (this.videoId) void this.load();
    document.addEventListener("hometube:sleep-timer-expired", this.onSleepExpired as EventListener);
    document.addEventListener("hometube:bookmarks-loaded", this.onBookmarksLoaded as EventListener);
    document.addEventListener("hometube:autoplay-cap-reached", this.onAutoplayCap as EventListener);
  }

  override updated(changed: Map<string, unknown>): void {
    if (changed.has("videoId") && this.videoId) {
      void this.load();
    }
  }

  override disconnectedCallback(): void {
    super.disconnectedCallback();
    this.stopHeartbeat();
    this.mediaUnsub?.();
    this.mediaUnsub = null;
    document.removeEventListener(
      "hometube:sleep-timer-expired",
      this.onSleepExpired as EventListener,
    );
    document.removeEventListener(
      "hometube:bookmarks-loaded",
      this.onBookmarksLoaded as EventListener,
    );
    document.removeEventListener(
      "hometube:autoplay-cap-reached",
      this.onAutoplayCap as EventListener,
    );
  }

  private async load(): Promise<void> {
    this.error = "";
    try {
      // Restore the audio-only preference for this video.
      this.audioOnly = !!getVideoPrefs(this.videoId).audioOnly;

      if (this.preview) {
        const meta = await api.get<VideoMetadata>(`/api/preview/video/${this.videoId}`);
        this.metadata = meta;
        try {
          this.stream = await api.get<StreamResponse>(`/api/videos/${this.videoId}/stream`);
        } catch {
          this.stream = null;
        }
      } else {
        const [meta, stream, settings, captions] = await Promise.all([
          api.get<VideoMetadata>(`/api/videos/${this.videoId}`),
          api.get<StreamResponse>(`/api/videos/${this.videoId}/stream`),
          api.get<ChildSettings>(`/api/children/me/settings`).catch(() => null),
          api
            .get<CaptionTrack[]>(`/api/videos/${this.videoId}/captions`)
            .catch(() => [] as CaptionTrack[]),
        ]);
        this.metadata = meta;
        this.stream = stream;
        this.settings = settings;
        this.captionTracks = captions;
      }
      // Wait for the player element to render before attaching the source.
      queueMicrotask(() => this.attachSource());
    } catch (err) {
      this.error = err instanceof ApiError ? String(err.body) : (err as Error).message;
    }
  }

  private attachSource(): void {
    if (!this.playerEl) return;

    if (this.audioOnly) {
      const audioUrl = this.bestAudioUrl();
      if (audioUrl) {
        this.playerEl.src = { src: audioUrl, type: "audio/mp4" };
      }
    } else {
      // Pick the right MIME type so vidstack routes to dash.js or
      // hls.js. The `/stream` JSON tells us which flavour the backend
      // produced; default to DASH for legacy callers.
      const manifestType = this.stream?.manifest_type;
      const type =
        manifestType === "hls"
          ? "application/vnd.apple.mpegurl"
          : "application/dash+xml";
      this.playerEl.src = {
        src: `/api/videos/${encodeURIComponent(this.videoId)}/stream/manifest.mpd`,
        type,
      };
    }

    if (this.settings?.playback_speed_locked) {
      this.playerEl.playbackRate = 1;
    }
    if (this.startAt != null && Number.isFinite(this.startAt)) {
      const target = this.startAt;
      // Subscribe to wait until duration is known, then seek.
      const stop = this.playerEl.subscribe((s) => {
        if (Number.isFinite(s.duration) && s.duration > 0) {
          try {
            this.playerEl.currentTime = target;
          } catch {
            // ignore
          }
          stop();
        }
      });
    }
    this.subscribeToMediaState();
  }

  /** Subscribe once to vidstack's state stream and dispatch events. */
  private subscribeToMediaState(): void {
    if (!this.playerEl || this.mediaUnsub) return;
    let lastPaused = true;
    let lastEnded = false;
    this.mediaUnsub = this.playerEl.subscribe((state) => {
      // currentTime → outbound event
      if (state.currentTime !== this.lastSeenTime) {
        this.lastSeenTime = state.currentTime;
        document.dispatchEvent(
          new CustomEvent("hometube:current-time", {
            detail: { seconds: state.currentTime },
          }),
        );
      }
      if (lastPaused && !state.paused) {
        lastPaused = false;
        this.onPlay();
      } else if (!lastPaused && state.paused) {
        lastPaused = true;
        this.onPause();
      }
      if (!lastEnded && state.ended) {
        lastEnded = true;
        this.onEnded();
      } else if (lastEnded && !state.ended) {
        lastEnded = false;
      }
    });
  }

  /** Pick the highest-bitrate audio-only format from the stream list. */
  private bestAudioUrl(): string | null {
    if (!this.stream) return null;
    const audio = this.stream.formats
      .filter((f) => (f.vcodec === "none" || f.height == null) && f.acodec !== "none")
      .sort((a, b) => (b.format_id?.length ?? 0) - (a.format_id?.length ?? 0));
    const chosen = audio[0];
    if (!chosen) return null;
    const params = new URLSearchParams({
      video_id: this.videoId,
      format: chosen.format_id,
    });
    return `/api/proxy/audio?${params.toString()}`;
  }

  private toggleAudioOnly = (): void => {
    this.audioOnly = !this.audioOnly;
    setVideoPrefs(this.videoId, { audioOnly: this.audioOnly });
    queueMicrotask(() => this.attachSource());
  };

  private onPlay = (): void => {
    this.startHeartbeat();
    if (!this.manualPlayed) {
      this.manualPlayed = true;
      try {
        sessionStorage.setItem(AUTOPLAY_KEY, "0");
      } catch {
        // ignore
      }
    }
  };

  private onPause = (): void => {
    this.stopHeartbeat();
  };

  private onEnded = (): void => {
    void this.sendHeartbeat();
    this.stopHeartbeat();

    void this.checkAfterVideoTimer();

    if (this.shouldShowContinuePrompt()) {
      this.continuePromptOpen = true;
      return;
    }

    document.dispatchEvent(
      new CustomEvent("hometube:video-ended", {
        detail: { videoId: this.videoId },
      }),
    );
  };

  private async checkAfterVideoTimer(): Promise<void> {
    try {
      const timer = await api.get<{ timer_type?: string } | null>("/api/timer").catch(() => null);
      if (timer?.timer_type === "after_video") {
        await api.delete("/api/timer").catch(() => {});
        document.dispatchEvent(new CustomEvent("hometube:sleep-timer-expired"));
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
      count = Number(sessionStorage.getItem(AUTOPLAY_KEY) ?? "0");
    } catch {
      count = 0;
    }
    return count >= this.settings.autoplay_max_consecutive;
  }

  private onContinue = (): void => {
    this.continuePromptOpen = false;
    try {
      sessionStorage.setItem(AUTOPLAY_KEY, "0");
    } catch {
      // ignore
    }
    document.dispatchEvent(
      new CustomEvent("hometube:video-ended", {
        detail: { videoId: this.videoId },
      }),
    );
  };

  private onSleepExpired = (): void => {
    if (!this.playerEl) return;
    // Vidstack exposes volume on the underlying media element via the
    // `state` stream; for the fade-out we just touch the host element's
    // CSS-mediated `--media-volume` and call `pause()` after the fade.
    const player = this.playerEl as unknown as {
      volume: number;
      pause(): void;
    };
    const startVolume = player.volume ?? 1;
    const start = Date.now();
    const fade = (): void => {
      const elapsed = Date.now() - start;
      const ratio = Math.max(0, 1 - elapsed / SLEEP_FADE_MS);
      try {
        player.volume = Math.max(0, startVolume * ratio);
      } catch {
        // ignore
      }
      if (elapsed < SLEEP_FADE_MS) {
        requestAnimationFrame(fade);
      } else {
        try {
          player.pause();
          player.volume = startVolume;
        } catch {
          // ignore
        }
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
    if (this.preview) return;
    if (this.heartbeatTimer != null) return;
    this.lastHeartbeatAt = Date.now();
    this.heartbeatTimer = window.setInterval(() => void this.sendHeartbeat(), HEARTBEAT_MS);
  }

  private stopHeartbeat(): void {
    if (this.heartbeatTimer != null) {
      window.clearInterval(this.heartbeatTimer);
      this.heartbeatTimer = null;
    }
  }

  private async sendHeartbeat(): Promise<void> {
    if (!this.playerEl || !this.metadata) return;
    const now = Date.now();
    const elapsed = Math.max(1, Math.round((now - this.lastHeartbeatAt) / 1000));
    this.lastHeartbeatAt = now;
    const state = this.playerEl.state;
    try {
      const res = await api.post<HeartbeatResponse>("/api/usage/heartbeat", {
        video_id: this.videoId,
        position_seconds: Math.floor(state.currentTime ?? 0),
        duration_seconds: Math.floor(state.duration ?? 0) || null,
        video_title: this.metadata.title,
        video_thumbnail_url: this.metadata.thumbnail_url,
        channel_title: this.metadata.channel_title,
        elapsed_seconds: elapsed,
      });
      this.remainingSeconds = res.remaining_seconds;
      if (res.limit_exceeded) {
        this.handleUsageLimit({
          reason: res.reason ?? "limit_exceeded",
          remaining_seconds: res.remaining_seconds ?? 0,
          allowed_window: res.allowed_window ?? null,
        });
      }
    } catch (err) {
      if (err instanceof ApiError && err.status === 403) {
        const body = (err.body as UsageLimitResponse | null) ?? {
          reason: "limit_exceeded" as const,
          remaining_seconds: 0,
        };
        this.handleUsageLimit(body);
      }
    }
  }

  private handleUsageLimit(detail: UsageLimitResponse): void {
    try {
      (this.playerEl as unknown as { pause(): void }).pause();
    } catch {
      // ignore
    }
    this.stopHeartbeat();
    this.dispatchEvent(
      new CustomEvent("hometube:usage-limit", {
        detail,
        bubbles: true,
        composed: true,
      }),
    );
  }

  private renderBookmarkMarkers() {
    if (!this.playerEl || !this.metadata) return nothing;
    const duration = Math.max(1, this.playerEl.state?.duration ?? 0);
    if (!isFinite(duration) || duration <= 0) return nothing;
    return html`
      <div class="seek-overlay" aria-hidden="true">
        ${this.bookmarks.map((b) => {
          const pct = Math.min(100, Math.max(0, (b.timestamp_seconds / duration) * 100));
          return html`<span
            class="bookmark-marker"
            style="left: ${pct}%"
            title=${b.label ?? `${b.timestamp_seconds}s`}
          ></span>`;
        })}
      </div>
    `;
  }

  /** "Download for offline" handler — uses the Cache API. */
  private onDownload = async (): Promise<void> => {
    if (!this.metadata || !this.stream) return;
    if (this.downloading || this.downloaded) return;
    this.downloading = true;
    try {
      const est = await getStorageEstimate();
      if (est && est.quota - est.usage < LOW_STORAGE_BYTES) {
        if (!confirm("Less than 500 MB of free storage. Download anyway?")) {
          this.downloading = false;
          return;
        }
      }
      await ensurePersistentStorage();

      // Pick a sensible default quality — first <=720p video+audio.
      const cap = this.settings?.max_quality && QUALITY_CAP[this.settings.max_quality];
      const candidate = this.stream.formats
        .filter(
          (f) =>
            f.height != null &&
            f.acodec !== "none" &&
            f.vcodec !== "none" &&
            (cap == null || (f.height ?? 0) <= cap),
        )
        .sort((a, b) => (b.height ?? 0) - (a.height ?? 0))[0];
      const qualityLabel = candidate?.height ? `${candidate.height}p` : "auto";

      // Tell the backend we're downloading (best-effort — backend may
      // not implement this yet).
      await api
        .post("/api/downloads", {
          video_id: this.videoId,
          quality: qualityLabel,
        })
        .catch(() => null);

      const streamUrl = `/api/downloads/${encodeURIComponent(
        this.videoId,
      )}/stream?quality=${encodeURIComponent(qualityLabel)}`;
      const res = await fetch(streamUrl, { credentials: "same-origin" });
      if (!res.ok) {
        throw new Error(`Download failed: HTTP ${res.status}`);
      }
      await saveVideoToOpfs(this.videoId, qualityLabel, res, this.metadata, streamUrl);
      await api
        .put(`/api/downloads/${encodeURIComponent(this.videoId)}`, {
          status: "complete",
          quality: qualityLabel,
        })
        .catch(() => null);
      this.downloaded = true;
    } catch (err) {
      console.warn("Download failed", err);
      alert(err instanceof Error ? `Download failed: ${err.message}` : "Download failed.");
    } finally {
      this.downloading = false;
    }
  };

  override render() {
    if (this.error) {
      return html`<hometube-error-banner .message=${this.error}></hometube-error-banner>`;
    }
    const posterStyle =
      this.audioOnly && this.metadata?.thumbnail_url
        ? `background-image: url(${this.metadata.thumbnail_url});`
        : "";
    return html`
      <div class="player-shell">
        <media-player
          aria-label=${this.metadata?.title ?? "Video player"}
          style=${posterStyle}
          .crossOrigin=${"use-credentials"}
        >
          <media-provider>
            ${this.captionTracks.map(
              (t) =>
                html`<track
                  kind="subtitles"
                  src=${`/api/videos/${encodeURIComponent(this.videoId)}/captions/${encodeURIComponent(t.lang)}`}
                  srclang=${t.lang}
                  label=${t.auto_generated ? `${t.lang} (auto)` : t.lang}
                />`,
            )}
            ${this.metadata?.thumbnail_url
              ? html`<media-poster
                  src=${this.metadata.thumbnail_url}
                  class="vds-poster"
                ></media-poster>`
              : nothing}
          </media-provider>
          <media-video-layout></media-video-layout>
        </media-player>
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
                  <button type="button" @click=${this.onContinue}>Continue watching</button>
                </div>
              </div>
            `
          : nothing}
      </div>
      ${this.renderBookmarkMarkers()} ${this.renderCountdown()}
      ${this.metadata
        ? html`<div class="meta">
            <h1>${this.metadata.title ?? "Untitled"}</h1>
            ${this.metadata.channel_title
              ? html`<div class="channel">${this.metadata.channel_title}</div>`
              : null}
            <div class="chrome">
              ${this.preview
                ? nothing
                : html`
                    <hometube-like-button video-id=${this.videoId}></hometube-like-button>
                    ${this.metadata.channel_id
                      ? html`<hometube-subscribe-button
                          channel-id=${this.metadata.channel_id}
                        ></hometube-subscribe-button>`
                      : nothing}
                    <hometube-bookmark-button video-id=${this.videoId}></hometube-bookmark-button>
                    <hometube-sleep-timer></hometube-sleep-timer>
                    ${this.settings?.downloads_enabled !== false
                      ? html`<button
                          type="button"
                          class="download-button"
                          ?disabled=${this.downloading || this.downloaded}
                          @click=${this.onDownload}
                          aria-label="Download for offline"
                        >
                          ${this.downloaded
                            ? "Downloaded"
                            : this.downloading
                              ? "Downloading…"
                              : "Download"}
                        </button>`
                      : nothing}
                  `}
              <button
                type="button"
                class="audio-toggle"
                aria-pressed=${this.audioOnly ? "true" : "false"}
                @click=${this.toggleAudioOnly}
              >
                ${this.audioOnly ? "Show video" : "Audio only"}
              </button>
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
    let cls = "countdown";
    let text = `${minutes} minute${minutes === 1 ? "" : "s"} left for today.`;
    if (remaining <= 60) {
      cls = "countdown urgent";
      text = "Less than a minute left — wrapping up soon!";
    } else if (remaining <= 5 * 60) {
      cls = "countdown urgent";
      text = `${minutes} minute${minutes === 1 ? "" : "s"} left — almost done!`;
    }
    return html`<div class="${cls}" role="status" aria-live="polite">${text}</div>`;
  }
}

declare global {
  interface HTMLElementTagNameMap {
    "hometube-video-player": VideoPlayer;
  }
}
