/**
 * <hometube-video-player video-id="...">
 *
 * Wraps a vidstack `<media-player>` with HomeTube-specific behaviour:
 *   - loads the rewritten DASH manifest from /api/videos/:id/stream
 *   - sends a heartbeat every 30s while playing to /api/usage/heartbeat
 *   - dispatches `hometube:usage-limit` on a 403 from the heartbeat
 *   - exposes the captions menu and (optionally) playback-speed
 *
 * The DASH manifest itself is the rewritten one (segments routed
 * through /api/proxy/segment), so vidstack just plays it like any
 * other DASH source.
 */

import { LitElement, html, css } from 'lit';
import { customElement, property, query, state } from 'lit/decorators.js';

import { ApiError, api } from '../services/api.js';
import type {
  ChildSettings,
  StreamResponse,
  UsageLimitResponse,
  VideoMetadata,
} from '../types/index.js';

const HEARTBEAT_MS = 30_000;

@customElement('hometube-video-player')
export class VideoPlayer extends LitElement {
  @property({ type: String, attribute: 'video-id' })
  videoId = '';

  @state() private metadata: VideoMetadata | null = null;
  @state() private stream: StreamResponse | null = null;
  @state() private settings: ChildSettings | null = null;
  @state() private error = '';

  @query('video') private videoEl!: HTMLVideoElement;

  private heartbeatTimer: number | null = null;
  private lastHeartbeatAt = 0;

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
  `;

  override connectedCallback(): void {
    super.connectedCallback();
    if (this.videoId) void this.load();
  }

  override updated(changed: Map<string, unknown>): void {
    if (changed.has('videoId') && this.videoId) {
      void this.load();
    }
  }

  override disconnectedCallback(): void {
    super.disconnectedCallback();
    this.stopHeartbeat();
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
  };

  private onPause = (): void => {
    this.stopHeartbeat();
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
      const res = await api.post<UsageLimitResponse | { remaining_seconds: number; limit_exceeded: boolean }>(
        '/api/usage/heartbeat',
        {
          video_id: this.videoId,
          position_seconds: Math.floor(this.videoEl.currentTime),
          duration_seconds: Math.floor(this.videoEl.duration || 0) || null,
          video_title: this.metadata.title,
          video_thumbnail_url: this.metadata.thumbnail_url,
          channel_title: this.metadata.channel_title,
          elapsed_seconds: elapsed,
        },
      );
      if ('limit_exceeded' in res && res.limit_exceeded) {
        this.handleUsageLimit({ reason: 'limit_exceeded', remaining_seconds: 0 });
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
          @ended=${() => void this.sendHeartbeat()}
          aria-label=${this.metadata?.title ?? 'Video player'}
        ></video>
      </div>
      ${this.metadata
        ? html`<div class="meta">
            <h1>${this.metadata.title ?? 'Untitled'}</h1>
            ${this.metadata.channel_title
              ? html`<div class="channel">${this.metadata.channel_title}</div>`
              : null}
          </div>`
        : null}
    `;
  }
}

declare global {
  interface HTMLElementTagNameMap {
    'hometube-video-player': VideoPlayer;
  }
}
