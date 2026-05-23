/**
 * Shared types for HomeTube frontend components.
 *
 * Mirrors the JSON shapes returned by the backend API. Kept deliberately
 * narrow — only the fields the UI actually consumes.
 */

export type AccountType = "parent" | "child";

export interface Account {
  id: number;
  display_name: string;
  avatar_url: string | null;
  account_type: AccountType;
}

export interface AccountSummary {
  id: number;
  display_name: string;
  avatar_url: string | null;
  account_type: "parent" | "child";
  has_pin: boolean;
  created_at: number;
}

export interface AllowlistedChannel {
  id: number;
  channel_id: string;
  channel_title: string;
  channel_thumbnail_url: string | null;
  created_at: number;
}

export interface AllowlistedVideo {
  id: number;
  video_id: string;
  video_title: string;
  video_thumbnail_url: string | null;
  channel_title: string | null;
  created_at: number;
}

export interface BlockedVideo {
  id: number;
  video_id: string;
  video_title: string | null;
  reason: string | null;
  created_at: number;
}

export interface HiddenVideo {
  id: number;
  video_id: string;
  video_title: string | null;
  channel_id: string | null;
  channel_title: string | null;
  video_thumbnail_url: string | null;
  duration_seconds: number | null;
  hidden_at: number;
}

export interface ChildSettings {
  child_account_id: number;
  downloads_enabled: boolean;
  max_quality: "480p" | "720p" | "1080p" | null;
  playback_speed_locked: boolean;
  autoplay_enabled: boolean;
  autoplay_max_consecutive: number | null;
  /**
   * When true, the player loads the Chromecast SDK and surfaces a
   * cast button. The setting is the *cosmetic* gate; the real
   * enforcement is server-side — the backend only mints a
   * `cast_manifest_url` (in `StreamResponse`) when this is true, so a
   * tampered UI can't smuggle a kid past the parental control.
   */
  chromecast_enabled: boolean;
}

export interface SearchItem {
  kind: "channel" | "video";
  id: string;
  title: string;
  description: string;
  channel_id: string | null;
  channel_title: string | null;
  thumbnails: Record<string, { url: string; width?: number; height?: number }>;
  published_at: string | null;
}

export interface SearchResponse {
  items: SearchItem[];
}

// ---------------------------------------------------------------------------
// Phase 10 — child-side allowlist-bounded search
// ---------------------------------------------------------------------------

/**
 * One channel result in `/api/search`. Returns from any channel the
 * child can reach via the allowlist or their subscriptions.
 */
export interface ChildSearchChannelHit {
  channel_id: string;
  channel_title: string;
  channel_thumbnail_url: string | null;
}

export interface ChildSearchVideoHit {
  video_id: string;
  title: string;
  channel_id: string | null;
  channel_title: string | null;
  thumbnail_url: string | null;
}

export interface ChildSearchResults {
  channels: ChildSearchChannelHit[];
  videos: ChildSearchVideoHit[];
}

export interface ChildSearchResponse {
  q: string;
  kind: "all" | "channel" | "video" | string;
  results: ChildSearchResults;
  next_page_token: string | null;
}

export interface ContinueWatchingItem {
  video_id: string;
  video_title: string;
  video_thumbnail_url: string | null;
  channel_title: string | null;
  duration_seconds: number | null;
  progress_seconds: number;
  last_watched_at: number;
}

export interface WatchAgainItem {
  video_id: string;
  video_title: string;
  video_thumbnail_url: string | null;
  channel_title: string | null;
  duration_seconds: number | null;
  last_watched_at: number;
}

export interface NewVideoItem {
  video_id: string;
  title: string;
  channel_id: string | null;
  channel_title: string | null;
  thumbnail_url: string | null;
  published_at: string | null;
  source_kind: "channel";
  source_id: string;
}

export interface VideoMetadata {
  id: string;
  title: string | null;
  channel_id: string | null;
  channel_title: string | null;
  duration_seconds: number | null;
  thumbnail_url: string | null;
}

export interface StreamResponse {
  video_id: string;
  manifest: string | null;
  /** Manifest flavour. Always "dash" when manifest is present. */
  manifest_type?: "dash";
  formats: Array<{
    format_id: string;
    ext?: string | null;
    height?: number | null;
    width?: number | null;
    fps?: number | null;
    vcodec?: string | null;
    acodec?: string | null;
    url?: string | null;
    protocol?: string | null;
  }>;
  /** Pre-signed proxy URL for audio-only playback. */
  audio_proxy_url?: string | null;
  /**
   * Short-lived signed manifest URL safe to hand to a Chromecast
   * receiver (no cookie auth required). Present only when the
   * requesting child has `chromecast_enabled = true`; absent otherwise.
   * Used to swap the player's loaded URL just before casting so the
   * receiver fetches a self-contained URL it can authenticate via
   * HMAC signature rather than cookies.
   */
  cast_manifest_url?: string | null;
  /** True when the video uses spherical/equirectangular projection (360°). */
  is_spherical?: boolean;
}

// ---------------------------------------------------------------------------
// Phase 7-9 types
// ---------------------------------------------------------------------------

export interface ChannelInfo {
  id: string;
  title: string;
  description: string;
  thumbnails: Record<string, { url: string; width?: number; height?: number }>;
  video_count: number | null;
}

export interface ChannelVideoItem {
  video_id: string;
  title: string;
  channel_id: string | null;
  channel_title: string | null;
  thumbnails: Record<string, { url: string; width?: number; height?: number }>;
  published_at: string | null;
  position: number | null;
}

export interface ChannelVideosPage {
  items: ChannelVideoItem[];
  next_page_token: string | null;
}

export interface SubscriptionRow {
  id: number;
  channel_id: string;
  channel_title: string;
  channel_thumbnail_url: string | null;
  subscribed_at: number;
  visible: boolean;
}

export interface LikeRow {
  id: number;
  video_id: string;
  video_title: string | null;
  video_thumbnail_url: string | null;
  /**
   * Channel the video belongs to. Captured at like-time from the
   * player's metadata so cards on `/child/liked` can render the channel
   * name and so the `visible` flag can match against allowlisted
   * channels. May be `null` for likes recorded before this field
   * existed (migration `014`).
   */
  channel_id: string | null;
  channel_title: string | null;
  /**
   * Video length in seconds. Captured at like-time so the
   * `/child/liked` grid can render a duration badge without a follow-up
   * metadata fetch. May be `null` for likes recorded before migration
   * `019` or when the player couldn't determine the duration.
   */
  duration_seconds: number | null;
  liked_at: number;
  /**
   * `true` when the liked video is reachable through the child's
   * allowlist (direct video allowlist; `video_likes` doesn't carry
   * channel metadata). Likes for videos the parent hasn't
   * allowlisted come back with `visible: false` so the child UI can
   * drop them.
   */
  visible: boolean;
}

export interface UpNextItem {
  video_id: string;
  title: string;
  channel_id: string | null;
  channel_title: string | null;
  thumbnail_url: string | null;
}

export interface SleepTimerRow {
  id: number;
  timer_type: "after_video" | "minutes";
  minutes_remaining: number | null;
  videos_remaining: number | null;
  started_at: number;
  expires_at: number | null;
}

/** Best-effort thumbnail-pick helper used across components. */
export function pickThumbnail(thumbs: Record<string, { url: string }>): string | null {
  for (const key of ["maxres", "high", "standard", "medium", "default"]) {
    const t = thumbs[key];
    if (t) return t.url;
  }
  return null;
}

// ---------------------------------------------------------------------------
// Phase 17 — activity + notifications
// ---------------------------------------------------------------------------

export interface ActivitySummary {
  period: string;
  total_seconds: number;
  videos_watched: number;
  sessions: number;
  daily_minutes: { date: string; minutes: number }[];
}

export interface ActivityHistoryEntry {
  video_id: string;
  video_title: string | null;
  video_thumbnail_url: string | null;
  channel_title: string | null;
  started_at: number;
  ended_at: number | null;
  duration_seconds: number | null;
}

export interface TopChannel {
  channel_title: string | null;
  total_seconds: number;
  videos_watched: number;
}

export interface SearchLogEntry {
  id: number;
  query: string;
  result_count: number;
  searched_at: number;
}

export type NotificationType = "ytdlp_failure" | "new_search_term" | "system_update";

export interface NotificationRow {
  id: number;
  notification_type: NotificationType;
  title: string;
  message: string;
  metadata: string | null;
  is_read: number;
  created_at: number;
}

// ---------------------------------------------------------------------------
// Phase 16 — preview
// ---------------------------------------------------------------------------

export interface ChannelPreview {
  id: string;
  title: string;
  description: string;
  thumbnails: Record<string, { url: string; width?: number; height?: number }>;
  video_count: number | null;
  videos: ChannelVideoItem[];
  next_page_token: string | null;
}

// ---------------------------------------------------------------------------
// Phase 16 — caption track listing
// ---------------------------------------------------------------------------

export interface CaptionTrack {
  lang: string;
  name: string | null;
  auto_generated: boolean;
}
