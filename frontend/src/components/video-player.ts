/**
 * <hometube-video-player video-id="...">
 *
 * Wraps a native `<video>` element with shaka-player (Google's
 * open-source adaptive streaming library) and adds HomeTube-specific
 * behaviour:
 *
 *   - Loads metadata + DASH-manifest URL from
 *     `/api/videos/:id/stream`. The backend exposes the synthesized
 *     manifest at `/api/videos/:id/stream/manifest.mpd`.
 *   - Sends a heartbeat every 30s while playing to
 *     `/api/usage/heartbeat`; dispatches `hometube:usage-limit` on a
 *     403.
 *   - Dispatches `hometube:current-time` on every `timeupdate` and
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
 *
 * shaka-player handles DASH (including webm SegmentBase) and HLS
 * natively. Its built-in UI overlay provides quality switching,
 * language selection, fullscreen, PiP, and captions.
 */

import { LitElement, html, css, nothing } from "lit";
import { customElement, property, query, state } from "lit/decorators.js";

// shaka-player's pre-built UI bundle (includes player + UI overlay).
// The build uses a UMD-like wrapper that exports via `exports` when
// available (which Vite's bundler provides), making it importable
// as a default/namespace import.
// @ts-ignore — shaka-player doesn't ship proper ESM type mappings
import * as shaka from "shaka-player/dist/shaka-player.ui.js";
import shakaControlsCss from "shaka-player/dist/controls.css?inline";

// Minimal type declarations for the shaka APIs we use. The actual
// runtime objects are full-featured; we only type what we touch.
interface ShakaPlayer {
  attach(video: HTMLVideoElement): Promise<void>;
  load(uri: string, startTime?: number, mimeType?: string): Promise<void>;
  destroy(): Promise<void>;
  configure(path: string, value: unknown): void;
  getNetworkingEngine(): {
    registerRequestFilter(
      filter: (type: number, request: { allowCrossSiteCredentials: boolean }) => void,
    ): void;
  } | null;
  addEventListener(event: string, handler: (e: Event) => void): void;
  addTextTrackAsync(
    uri: string,
    language: string,
    kind: string,
    mimeType: string,
    codec?: string,
    label?: string,
  ): Promise<unknown>;
  setTextVisibility(visible: boolean): void;
  isTextVisible(): boolean;
}
interface ShakaUI {
  configure(config: Record<string, unknown>): void;
  destroy(): void;
}

// Cast the imported namespace to our typed shape.
const Shaka = shaka as unknown as {
  polyfill: { installAll(): void };
  Player: new () => ShakaPlayer;
  ui: {
    Overlay: new (player: ShakaPlayer, container: HTMLElement, video: HTMLVideoElement) => ShakaUI;
  };
};

import { ApiError, api } from "../services/api.js";
import {
  ensurePersistentStorage,
  getStorageEstimate,
  saveVideoToOpfs,
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
const VOLUME_KEY = "hometube-volume";
const AUDIO_ONLY_KEY = "hometube-audio-only";
const CAPTIONS_KEY = "hometube-captions";
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

/**
 * Read an EBML variable-width integer at `offset`.
 * Returns `[width, value]` or `null` if the data is too short/invalid.
 */
function readEbmlVint(buf: Uint8Array, offset: number): [number, number] | null {
  if (offset >= buf.length) return null;
  const first = buf[offset];
  if (first === 0) return null;
  let width = 1;
  let mask = 0x80;
  while ((first & mask) === 0 && width < 8) {
    width++;
    mask >>= 1;
  }
  if (offset + width > buf.length || width > 8) return null;
  let value = first & (mask - 1);
  for (let i = 1; i < width; i++) {
    value = value * 256 + buf[offset + i];
  }
  return [width, value];
}

/** Projection metadata extracted from a WebM init segment. */
interface ProjectionInfo {
  /** ProjectionType: 0=rect, 1=equirectangular, 2=cubemap, 3=equi+bounds. */
  type: number;
  /** ProjectionPoseYaw in degrees (IEEE 754 float). */
  yaw: number;
  /** ProjectionPosePitch in degrees (IEEE 754 float). */
  pitch: number;
  /** ProjectionPoseRoll in degrees (IEEE 754 float). */
  roll: number;
}

/**
 * Read a 32-bit big-endian IEEE 754 float from `buf` at `offset`.
 */
function readFloat32BE(buf: Uint8Array, offset: number): number {
  if (offset + 4 > buf.length) return 0;
  const dv = new DataView(buf.buffer, buf.byteOffset + offset, 4);
  return dv.getFloat32(0, false);
}

// WebM Projection sub-element IDs (2-byte VINT each):
//   ProjectionType       0x7671
//   ProjectionPrivate    0x7672
//   ProjectionPoseYaw    0x7673
//   ProjectionPosePitch  0x7674
//   ProjectionPoseRoll   0x7675

/**
 * Parse the children of a Projection master element starting at
 * `dataStart` with `dataLen` bytes. Extracts type and pose values.
 */
function parseProjectionChildren(
  buf: Uint8Array,
  dataStart: number,
  dataLen: number,
): ProjectionInfo {
  const info: ProjectionInfo = { type: 1, yaw: 0, pitch: 0, roll: 0 };
  let pos = dataStart;
  const end = dataStart + dataLen;
  while (pos < end) {
    // Read element ID (2-byte for 0x76XX sub-elements).
    if (pos + 2 > end) break;
    const idHi = buf[pos];
    const idLo = buf[pos + 1];
    pos += 2;
    // Read size VINT.
    const sizeRead = readEbmlVint(buf, pos);
    if (!sizeRead) break;
    const [sw, sv] = sizeRead;
    pos += sw;
    if (pos + sv > end) break;
    if (idHi === 0x76) {
      if (idLo === 0x71) {
        // ProjectionType — unsigned integer (1 byte for typical values).
        info.type = sv === 1 ? buf[pos] : 0;
      } else if (idLo === 0x73 && sv === 4) {
        info.yaw = readFloat32BE(buf, pos);
      } else if (idLo === 0x74 && sv === 4) {
        info.pitch = readFloat32BE(buf, pos);
      } else if (idLo === 0x75 && sv === 4) {
        info.roll = readFloat32BE(buf, pos);
      }
    }
    pos += sv;
  }
  return info;
}

/**
 * Scan `buf` for a WebM Projection master element (ID 0x7670).
 * If found:
 * 1. Extract projection metadata (type, pose yaw/pitch/roll).
 * 2. Overwrite the element byte-for-byte with a Void element (0xEC)
 *    so parent size fields stay correct and MSE ignores it.
 *
 * Returns the extracted `ProjectionInfo` if a Projection element was
 * found and rewritten, or `null` if none was found.
 */
function extractAndStripProjection(buf: Uint8Array): ProjectionInfo | null {
  for (let i = 0; i + 4 < buf.length; i++) {
    if (buf[i] !== 0x76 || buf[i + 1] !== 0x70) continue;
    const sizeRead = readEbmlVint(buf, i + 2);
    if (!sizeRead) continue;
    const [sizeWidth, sizeValue] = sizeRead;
    const total = 2 + sizeWidth + sizeValue;
    if (i + total > buf.length) continue;
    // First child should be ProjectionType (0x7671).
    const childOffset = i + 2 + sizeWidth;
    if (buf[childOffset] !== 0x76 || buf[childOffset + 1] !== 0x71) {
      continue;
    }
    // Extract metadata before stripping.
    const info = parseProjectionChildren(buf, childOffset, sizeValue);
    // Rewrite as Void: 0xEC + 1-byte VINT size + zero padding.
    const voidDataLen = total - 2;
    if (voidDataLen > 126) continue;
    buf[i] = 0xec;
    buf[i + 1] = 0x80 | voidDataLen;
    for (let j = i + 2; j < i + total; j++) {
      buf[j] = 0;
    }
    return info;
  }
  return null;
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

  @query("video") private videoEl!: HTMLVideoElement;
  @query(".shaka-container") private containerEl!: HTMLElement;

  private heartbeatTimer: number | null = null;
  private lastHeartbeatAt = 0;
  /**
   * True once at least one regular 30s heartbeat has been sent for this
   * video. Used to gate the unload beacon: we only flush remaining
   * progress for videos that have already crossed the "really watched"
   * threshold, so quick previews don't pollute watch_history.
   */
  private heartbeatSent = false;
  /** Debounce timer for `seeked` events so scrubbing doesn't burst-POST. */
  private seekedTimer: number | null = null;
  /** True after a manual play interaction; resets the autoplay counter. */
  private manualPlayed = false;
  /** Last currentTime we saw — used to compute heartbeat deltas. */
  private lastSeenTime = 0;
  /** shaka.Player instance. */
  private player: ShakaPlayer | null = null;
  /** shaka.ui.Overlay instance (manages the control bar). */
  private ui: ShakaUI | null = null;
  /** Spherical (360°) renderer instance — created lazily for VR videos. */
  private sphericalRenderer: { destroy(): void; resize(): void } | null = null;
  /** Projection pose extracted from the first WebM init segment. */
  private projectionInfo: ProjectionInfo | null = null;

  static styles = [
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
      .shaka-container {
        width: 100%;
        aspect-ratio: 16 / 9;
        background: black;
        border-radius: 0.5rem;
        overflow: hidden;
        position: relative;
      }
      .shaka-container video {
        width: 100%;
        height: 100%;
        object-fit: contain;
      }
      /* 360° spherical canvas overlays the video surface. Shaka's
         controls (z-indexed higher) remain clickable on top. */
      .spherical-canvas {
        position: absolute;
        inset: 0;
        width: 100%;
        height: 100%;
        z-index: 0;
        cursor: grab;
      }
      .spherical-canvas:active {
        cursor: grabbing;
      }
      /* When the spherical canvas is active, hide the flat <video>. */
      :host([data-spherical]) .shaka-container video {
        opacity: 0;
        pointer-events: none;
      }
      /* Caption styling. Shaka uses native <track> / ::cue rendering
         via NativeTextDisplayer. The ::cue pseudo-element controls how
         WebVTT cues appear over the video. */
      video::cue {
        background-color: rgba(0, 0, 0, 0.8);
        color: white;
        font-size: clamp(0.85rem, 2.2vw, 1.2rem);
        line-height: 1.4;
        padding: 0.1em 0.3em;
        white-space: pre-wrap;
      }
      /* Audio-only mode: replace the player surface with the poster. */
      :host([data-audio-only]) .shaka-container {
        background-size: cover;
        background-position: center;
        background-repeat: no-repeat;
      }
      :host([data-audio-only]) .shaka-container video {
        opacity: 0;
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

  /** Guard against concurrent load() calls (connectedCallback + updated race). */
  private loadInFlight = false;

  override connectedCallback(): void {
    super.connectedCallback();
    // Inject shaka's control CSS into the shadow root via adoptedStyleSheets.
    this.injectShakaStyles();
    if (this.videoId) void this.load();
    document.addEventListener("hometube:sleep-timer-expired", this.onSleepExpired as EventListener);
    document.addEventListener("hometube:bookmarks-loaded", this.onBookmarksLoaded as EventListener);
    document.addEventListener("hometube:autoplay-cap-reached", this.onAutoplayCap as EventListener);
    // Flush progress when the page is hidden/unloaded (tab close,
    // navigation to next video, app backgrounded on mobile). Without
    // this, any progress accumulated since the last 30s heartbeat is
    // lost — which is why watch_history / continue-watching looked
    // inconsistent for short or quickly-navigated sessions.
    window.addEventListener("pagehide", this.onPageHide);
    document.addEventListener("visibilitychange", this.onVisibilityChange);
  }

  override updated(changed: Map<string, unknown>): void {
    if (changed.has("videoId") && this.videoId && !this.loadInFlight) {
      void this.load();
    }
  }

  override disconnectedCallback(): void {
    super.disconnectedCallback();
    this.stopHeartbeat();
    // Remove the `seeked` listener synchronously here (in addition to
    // `destroyPlayer`, which runs async) so a stray `seeked` between
    // disconnect and DOM teardown can't fire `sendProgress` on a
    // detached element.
    if (this.videoEl) {
      this.videoEl.removeEventListener("seeked", this.onSeeked);
    }
    // Reset the "really watched" gate and any pending seek-debounce so
    // a re-attached instance starts clean instead of inheriting stale
    // state from the previous video.
    this.heartbeatSent = false;
    void this.destroyPlayer();
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
    window.removeEventListener("pagehide", this.onPageHide);
    document.removeEventListener("visibilitychange", this.onVisibilityChange);
    if (this.seekedTimer != null) {
      window.clearTimeout(this.seekedTimer);
      this.seekedTimer = null;
    }
  }

  /** Inject shaka-player's controls.css into this shadow root. */
  private injectShakaStyles(): void {
    if (!this.shadowRoot) return;
    const sheet = new CSSStyleSheet();
    sheet.replaceSync(shakaControlsCss);
    this.shadowRoot.adoptedStyleSheets = [...this.shadowRoot.adoptedStyleSheets, sheet];
  }

  private async load(): Promise<void> {
    if (this.loadInFlight) return;
    this.loadInFlight = true;
    this.error = "";
    // Reset the "really watched" gate when the video changes.
    this.heartbeatSent = false;
    try {
      // Restore the audio-only preference (global, not per-video).
      try {
        this.audioOnly = localStorage.getItem(AUDIO_ONLY_KEY) === "true";
      } catch {
        this.audioOnly = false;
      }

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
      // Wait for the element to render before attaching the player.
      await this.updateComplete;
      await this.attachSource();
    } catch (err) {
      if (err instanceof ApiError) {
        this.error = String(err.body);
      } else if (err && typeof err === "object" && "code" in err && "category" in err) {
        // shaka.util.Error: surface the code, category, and data for debugging.
        const se = err as { code: number; category: number; data: unknown[] };
        this.error = `Shaka Error ${se.category}.${se.code}: ${JSON.stringify(se.data)}`;
      } else {
        this.error = (err as Error).message ?? String(err);
      }
    } finally {
      this.loadInFlight = false;
    }
  }

  private async attachSource(): Promise<void> {
    if (!this.videoEl || !this.containerEl) return;

    // Destroy any existing player instance (e.g. on videoId change).
    await this.destroyPlayer();

    // Install polyfills if needed (no-op in modern browsers).
    Shaka.polyfill.installAll();

    // Create player.
    const player = new Shaka.Player();
    await player.attach(this.videoEl);
    this.player = player;

    // Configure networking: send cookies with manifest/segment requests.
    player.getNetworkingEngine()!.registerRequestFilter((_type, request) => {
      request.allowCrossSiteCredentials = true;
    });

    // Rewrite WebM init segments to neutralize the Projection element.
    // YouTube serves 360°/VR videos as WebM with a `Projection` master
    // element inside the Video track. Chromium's MSE VP9 path rejects
    // those init segments (MEDIA_SOURCE_OPERATION_FAILED, error 3014).
    // Before stripping, we extract the projection metadata (type, pose
    // yaw/pitch/roll) so the spherical renderer can use it for the
    // initial camera direction.
    // Shaka's RequestType enum: MANIFEST=0, SEGMENT=1, LICENSE=2, ...
    const SHAKA_REQUEST_TYPE_SEGMENT = 1;
    (player.getNetworkingEngine() as any).registerResponseFilter(
      (type: number, response: { data: ArrayBuffer | Uint8Array }) => {
        if (type !== SHAKA_REQUEST_TYPE_SEGMENT) return;
        const data = response.data;
        if (!(data instanceof ArrayBuffer)) return;
        if (data.byteLength < 8 || data.byteLength > 8192) return;
        const u8 = new Uint8Array(data);
        // Only inspect things that look like a WebM init (EBML header).
        if (u8[0] !== 0x1a || u8[1] !== 0x45 || u8[2] !== 0xdf || u8[3] !== 0xa3) {
          return;
        }
        const info = extractAndStripProjection(u8);
        if (info) {
          // Keep the first extraction (each quality has the same pose).
          if (!this.projectionInfo) {
            this.projectionInfo = info;
          }
          // eslint-disable-next-line no-console
          console.info(
            "[video-player] neutralized Projection, pose:",
            info.yaw,
            info.pitch,
            info.roll,
          );
        }
      },
    );

    // Default audio language to English. Shaka picks any AdaptationSet
    // whose `lang` attribute starts with "en" (e.g. "en", "en-US",
    // "en-GB"). Users can switch via the language button in the
    // overflow menu when multiple languages are present.
    //
    // Shaka v5.1 replaced the flat `preferredAudioLanguage` with
    // `preferredAudio`, an *array* of preference objects (each may
    // specify language, role, channelCount, label, etc).
    player.configure("preferredAudio", [{ language: "en" }]);

    // Codec preference: VP9 + opus first, AVC1 + mp4a as fallback.
    // The synthesized DASH manifest emits both ladders as separate
    // AdaptationSets whenever YouTube provides them. We prefer VP9
    // because:
    //   - it reaches 4K (AVC1 caps at 1080p on YouTube),
    //   - it's ~30–50% more bandwidth-efficient at equal quality,
    //   - modern tablets (the typical HomeTube client) have hardware
    //     VP9 decode.
    // AVC1 is kept as a fallback so videos without a VP9 encode
    // (e.g. long-form uploads where YouTube hasn't backfilled VP9)
    // still play instead of degrading to audio-only.
    //
    // Setting this explicitly insulates us from shaka's internal
    // default heuristic, which has shifted between versions.
    player.configure("preferredVideoCodecs", ["vp09", "vp9", "avc1", "avc3"]);
    player.configure("preferredAudioCodecs", ["opus", "mp4a"]);

    // Apply quality cap if set.
    if (this.settings?.max_quality) {
      const maxHeight = QUALITY_CAP[this.settings.max_quality];
      if (maxHeight) {
        player.configure("abr.restrictions.maxHeight", maxHeight);
      }
    }

    // Create UI overlay (controls, quality menu, language menu, etc.).
    const ui = new Shaka.ui.Overlay(player, this.containerEl, this.videoEl);
    this.ui = ui;
    // YouTube-style control bar layout:
    //   left  → play, volume (mute + slider), time
    //   right → captions, settings (overflow), PiP, cast, fullscreen
    // A `spacer` between the two clusters pushes the right side to the
    // far edge.
    //
    // Captions live directly on the bar (not just the overflow menu)
    // so the "CC" button is one click away, matching YouTube. The
    // overflow_menu icon becomes the "settings gear" and hosts quality,
    // language, and playback-speed pickers.
    const overflowButtons = this.settings?.playback_speed_locked
      ? ["quality", "language", "captions"]
      : ["quality", "language", "captions", "playback_rate"];
    // Only surface the PiP button when the browser actually supports
    // Picture-in-Picture. Otherwise Shaka leaves an inert slot in the
    // control bar on iOS Safari (PWA mode) and Firefox Android, which
    // misaligns the right-side cluster.
    const supportsPip =
      typeof document !== "undefined" &&
      "pictureInPictureEnabled" in document &&
      (document as Document & { pictureInPictureEnabled?: boolean }).pictureInPictureEnabled ===
        true;
    ui.configure({
      controlPanelElements: [
        "play_pause",
        "mute",
        "volume",
        "time_and_duration",
        "spacer",
        "captions",
        "overflow_menu",
        ...(supportsPip ? ["picture_in_picture"] : []),
        "cast",
        "fullscreen",
      ],
      overflowMenuButtons: overflowButtons,
      // Tint the scrubber YouTube-red. `played` is the filled portion,
      // `buffered` is the ahead-of-playhead loaded region; the default
      // base track color is fine, so we leave it unset.
      seekBarColors: {
        played: "rgb(255, 0, 0)",
        buffered: "rgba(255, 255, 255, 0.4)",
      },
    });

    // Error handling.
    player.addEventListener("error", (event: Event) => {
      const detail = (event as CustomEvent).detail;
      console.error("shaka error", detail);
    });

    // Wire up HTML5 video events.
    this.videoEl.addEventListener("timeupdate", this.onTimeUpdate);
    this.videoEl.addEventListener("play", this.onPlay);
    this.videoEl.addEventListener("pause", this.onPause);
    this.videoEl.addEventListener("ended", this.onEnded);
    this.videoEl.addEventListener("seeked", this.onSeeked);
    this.videoEl.addEventListener("volumechange", this.onVolumeChange);

    // Restore persisted volume level.
    try {
      const saved = localStorage.getItem(VOLUME_KEY);
      if (saved != null) {
        const vol = parseFloat(saved);
        if (Number.isFinite(vol)) {
          this.videoEl.volume = Math.max(0, Math.min(1, vol));
        }
      }
    } catch {
      // localStorage unavailable — use browser default.
    }

    // Load the appropriate source.
    if (this.audioOnly) {
      const audioUrl = this.bestAudioUrl();
      if (audioUrl) {
        // Explicit mimeType prevents shaka from guessing based on the
        // proxy URL (which has no file extension).
        await player.load(audioUrl, undefined, "audio/webm");
      }
    } else {
      const manifestUrl = `/api/videos/${encodeURIComponent(this.videoId)}/stream/manifest.mpd`;
      // Explicitly specify the MIME type so shaka doesn't have to guess
      // from the URL or Content-Type header (which may be text/xml or
      // application/octet-stream depending on the server config).
      await player.load(manifestUrl, undefined, "application/dash+xml");
    }

    // Add caption tracks (non-fatal — some languages may 404).
    for (const t of this.captionTracks) {
      const trackUrl = `/api/videos/${encodeURIComponent(this.videoId)}/captions/${encodeURIComponent(t.lang)}`;
      try {
        await player.addTextTrackAsync(
          trackUrl,
          t.lang,
          // W3C TextTrackKind: "subtitles" or "captions" (NOT "subtitle").
          t.auto_generated ? "captions" : "subtitles",
          "text/vtt",
          undefined,
          t.auto_generated ? `${t.lang} (auto)` : t.lang,
        );
      } catch {
        // Caption track failed to load — not fatal for playback.
      }
    }

    // Restore caption visibility from prior session.
    try {
      if (localStorage.getItem(CAPTIONS_KEY) === "true" && this.videoEl.textTracks.length > 0) {
        this.videoEl.textTracks[0].mode = "showing";
      }
    } catch {
      // localStorage unavailable.
    }

    // Persist caption visibility via native textTracks change event.
    // shaka's NativeTextDisplayer toggles track.mode directly, so we
    // listen on the video element's textTracks collection rather than
    // relying on shaka's own "textchanged" event (which only fires
    // from shaka's internal text management path).
    this.videoEl.textTracks.addEventListener("change", this.onCaptionChange);

    // Seek to start position if specified.
    if (this.startAt != null && Number.isFinite(this.startAt)) {
      this.videoEl.currentTime = this.startAt;
    }

    // Lock playback rate if needed.
    if (this.settings?.playback_speed_locked) {
      this.videoEl.playbackRate = 1;
    }

    // Activate 360° spherical renderer for VR videos.
    if (this.stream?.is_spherical && !this.audioOnly) {
      this.activateSphericalRenderer();
    }

    // Autoplay: attempt to start playback immediately. If the browser
    // blocks it (autoplay policy requires user gesture for unmuted
    // video), silently fall back to paused — the kid taps play.
    try {
      await this.videoEl.play();
    } catch {
      // Autoplay blocked — that's fine.
    }
  }

  private async destroyPlayer(): Promise<void> {
    if (this.sphericalRenderer) {
      this.sphericalRenderer.destroy();
      this.sphericalRenderer = null;
      this.removeAttribute("data-spherical");
      // Remove the canvas element from the DOM.
      this.shadowRoot?.querySelector(".spherical-canvas")?.remove();
    }
    this.projectionInfo = null;
    if (this.videoEl) {
      this.videoEl.removeEventListener("timeupdate", this.onTimeUpdate);
      this.videoEl.removeEventListener("play", this.onPlay);
      this.videoEl.removeEventListener("pause", this.onPause);
      this.videoEl.removeEventListener("ended", this.onEnded);
      this.videoEl.removeEventListener("seeked", this.onSeeked);
      this.videoEl.removeEventListener("volumechange", this.onVolumeChange);
      this.videoEl.textTracks.removeEventListener("change", this.onCaptionChange);
    }
    if (this.ui) {
      this.ui.destroy();
      this.ui = null;
    }
    if (this.player) {
      try {
        await this.player.destroy();
      } catch {
        // Ignore errors during teardown.
      }
      this.player = null;
    }
  }

  /**
   * Lazy-load the Three.js spherical renderer and overlay a `<canvas>`
   * on top of the video element. Only called for 360° videos.
   */
  private async activateSphericalRenderer(): Promise<void> {
    try {
      const { createSphericalRenderer } = await import("./spherical-renderer.js");
      // Create a canvas inside the shaka container, between the video
      // and the Shaka UI controls overlay.
      const canvas = document.createElement("canvas");
      canvas.className = "spherical-canvas";
      // Insert canvas right after the <video> element so Shaka's
      // control overlay (appended later) sits on top.
      this.videoEl.insertAdjacentElement("afterend", canvas);

      const pose = this.projectionInfo;
      this.sphericalRenderer = createSphericalRenderer({
        video: this.videoEl,
        canvas,
        // Shaka's control overlay sits above the canvas in z-order,
        // so pointer events must be captured on the container instead.
        dragTarget: this.containerEl,
        initialYaw: pose?.yaw ?? 0,
        initialPitch: pose?.pitch ?? 0,
      });
      // Set host attribute so CSS can hide the flat <video>.
      this.setAttribute("data-spherical", "");
    } catch (err) {
      // eslint-disable-next-line no-console
      console.warn("[video-player] failed to load spherical renderer:", err);
    }
  }

  /** Pre-signed audio-only proxy URL from the stream response. */
  private bestAudioUrl(): string | null {
    return this.stream?.audio_proxy_url ?? null;
  }

  private toggleAudioOnly = (): void => {
    this.audioOnly = !this.audioOnly;
    try {
      localStorage.setItem(AUDIO_ONLY_KEY, String(this.audioOnly));
    } catch {
      // localStorage unavailable.
    }
    void this.attachSource();
  };

  private onVolumeChange = (): void => {
    if (!this.videoEl) return;
    try {
      localStorage.setItem(VOLUME_KEY, String(this.videoEl.volume));
    } catch {
      // localStorage unavailable.
    }
  };

  private onCaptionChange = (): void => {
    if (!this.videoEl) return;
    const tracks = this.videoEl.textTracks;
    const anyShowing = Array.from(tracks).some((t) => t.mode === "showing");
    try {
      localStorage.setItem(CAPTIONS_KEY, String(anyShowing));
    } catch {
      // localStorage unavailable.
    }
  };

  private onTimeUpdate = (): void => {
    if (!this.videoEl) return;
    const t = this.videoEl.currentTime;
    if (t !== this.lastSeenTime) {
      this.lastSeenTime = t;
      document.dispatchEvent(
        new CustomEvent("hometube:current-time", {
          detail: { seconds: t },
        }),
      );
    }
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
    // Flush the exact playhead so resume is accurate to 1s without
    // waiting for the next 30s heartbeat.
    void this.sendProgress();
  };

  private onSeeked = (): void => {
    // Debounce: `seeked` fires on every scrub-bar release, including
    // intermediate stops while the user drags. We only need the final
    // resting position.
    if (this.seekedTimer != null) window.clearTimeout(this.seekedTimer);
    this.seekedTimer = window.setTimeout(() => {
      this.seekedTimer = null;
      void this.sendProgress();
    }, 500);
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
    if (!this.videoEl) return;
    const startVolume = this.videoEl.volume;
    const start = Date.now();
    const fade = (): void => {
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

  /**
   * Build the common payload shape shared by the heartbeat, progress,
   * and beacon-flush endpoints. `elapsedSeconds` is only included when
   * we're crediting usage time (heartbeat + beacon); position-only
   * progress updates omit it so the server skips `usage_log`.
   */
  private buildUsagePayload(elapsedSeconds: number | null): Record<string, unknown> | null {
    if (!this.videoEl || !this.metadata) return null;
    const payload: Record<string, unknown> = {
      video_id: this.videoId,
      position_seconds: Math.floor(this.videoEl.currentTime ?? 0),
      duration_seconds: Math.floor(this.videoEl.duration ?? 0) || null,
      video_title: this.metadata.title,
      video_thumbnail_url: this.metadata.thumbnail_url,
      channel_title: this.metadata.channel_title,
    };
    if (elapsedSeconds != null) payload.elapsed_seconds = elapsedSeconds;
    return payload;
  }

  /**
   * Flush a final heartbeat when the page is hiding. Uses
   * `navigator.sendBeacon` because regular `fetch` is not guaranteed to
   * complete during unload. Without this, progress accumulated since
   * the last 30s tick is silently dropped when the user navigates to
   * the next video or closes the tab.
   */
  private onPageHide = (): void => {
    this.flushBeacon();
  };

  private onVisibilityChange = (): void => {
    if (document.visibilityState === "hidden") this.flushBeacon();
  };

  private flushBeacon(): void {
    if (this.preview) return;
    // Only flush if the user has already watched long enough for at
    // least one regular heartbeat to land. Otherwise quick previews
    // (open, glance, close in <30s) would still create watch_history
    // entries via the beacon.
    if (!this.heartbeatSent) return;
    const now = Date.now();
    const elapsed = Math.max(1, Math.round((now - this.lastHeartbeatAt) / 1000));
    // Skip if we just sent one — `pagehide` + `visibilitychange` can
    // both fire on the same transition.
    if (elapsed < 2) return;
    const payload = this.buildUsagePayload(elapsed);
    if (!payload) return;
    let queued = false;
    try {
      const blob = new Blob([JSON.stringify(payload)], { type: "application/json" });
      queued = navigator.sendBeacon?.("/api/usage/heartbeat", blob) ?? false;
    } catch {
      // Best-effort — nothing we can do during unload.
    }
    // Only advance `lastHeartbeatAt` when the beacon was actually
    // queued. Otherwise, if the page survives (e.g. visibility flipped
    // back to visible) the next heartbeat would under-count.
    if (queued) this.lastHeartbeatAt = now;
  }

  /**
   * Position-only update. Fires on pause/seeked/ended so the resume
   * point in `watch_history` stays accurate to ~1s without bumping
   * the 30s usage-time accounting. Gated by `heartbeatSent` so a
   * quick preview (open + immediate seek/close) doesn't create a
   * `watch_history` row.
   */
  private async sendProgress(): Promise<void> {
    if (this.preview) return;
    if (!this.heartbeatSent) return;
    const payload = this.buildUsagePayload(null);
    if (!payload) return;
    try {
      await api.post("/api/usage/progress", payload);
    } catch {
      // Best-effort — progress will get caught by the next heartbeat
      // or the unload beacon.
    }
  }

  private async sendHeartbeat(): Promise<void> {
    const now = Date.now();
    const elapsed = Math.max(1, Math.round((now - this.lastHeartbeatAt) / 1000));
    const payload = this.buildUsagePayload(elapsed);
    if (!payload) return;
    this.lastHeartbeatAt = now;
    try {
      const res = await api.post<HeartbeatResponse>("/api/usage/heartbeat", payload);
      this.remainingSeconds = res.remaining_seconds;
      // Mark that a real heartbeat has landed — gates sendProgress
      // and the unload beacon so quick previews don't write to
      // watch_history.
      this.heartbeatSent = true;
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
      this.videoEl?.pause();
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
    if (!this.videoEl || !this.metadata) return nothing;
    const duration = Math.max(1, this.videoEl.duration ?? 0);
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
        <div class="shaka-container" style=${posterStyle}>
          <video autoplay .poster=${this.metadata?.thumbnail_url ?? ""}></video>
        </div>
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
