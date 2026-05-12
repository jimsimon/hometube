/**
 * Shared types for HomeTube frontend components.
 *
 * Mirrors the JSON shapes returned by the backend API. Kept deliberately
 * narrow — only the fields the UI actually consumes.
 */

export type AccountType = "parent" | "child";

export interface Account {
  id: number;
  email: string;
  display_name: string;
  avatar_url: string | null;
  account_type: AccountType;
}

export interface AccountSummary {
  id: number;
  email: string;
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

export interface AllowlistedPlaylist {
  id: number;
  playlist_id: string;
  playlist_title: string;
  playlist_thumbnail_url: string | null;
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

export interface ChildSettings {
  child_account_id: number;
  downloads_enabled: boolean;
  max_quality: "480p" | "720p" | "1080p" | null;
  playback_speed_locked: boolean;
  autoplay_enabled: boolean;
  autoplay_max_consecutive: number | null;
}

export interface UsageLimit {
  day_of_week: number;
  max_hours: number;
  allowed_start_time: string;
  allowed_end_time: string;
}

export interface SearchItem {
  kind: "channel" | "playlist" | "video";
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

/**
 * Origin of a playlist hit. Mirrors the `source` column on the backend
 * struct and is used by `<hometube-search-results>` to render the
 * appropriate badge:
 *
 * - `allowlist` — playlist the parent allowlisted directly
 * - `own` — playlist the child created in HomeTube
 * - `family` — family playlist the parent created and shared
 */
export type ChildSearchPlaylistSource = "allowlist" | "own" | "family";

export interface ChildSearchPlaylistHit {
  playlist_id: string;
  playlist_title: string;
  playlist_thumbnail_url: string | null;
  source: ChildSearchPlaylistSource;
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
  playlists: ChildSearchPlaylistHit[];
  videos: ChildSearchVideoHit[];
}

export interface ChildSearchResponse {
  q: string;
  kind: "all" | "channel" | "playlist" | "video" | string;
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

export interface NewVideoItem {
  video_id: string;
  title: string;
  channel_id: string | null;
  channel_title: string | null;
  thumbnail_url: string | null;
  published_at: string | null;
  source_kind: "channel" | "playlist";
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
}

export interface AllowedWindow {
  start: string; // "HH:MM"
  end: string;
}

export interface UsageLimitResponse {
  reason: "limit_exceeded" | "outside_window";
  remaining_seconds: number;
  allowed_window?: AllowedWindow | null;
}

export interface HeartbeatResponse {
  remaining_seconds: number | null;
  allowed_window: AllowedWindow | null;
  limit_exceeded: boolean;
  reason?: "limit_exceeded" | "outside_window";
}

// ---------------------------------------------------------------------------
// Phase 7-9 types
// ---------------------------------------------------------------------------

export type SyncStatus =
  | "synced"
  | "pending_push"
  | "pending_delete"
  | "pending_create"
  | "pending_update"
  | "error";

export interface ChannelInfo {
  id: string;
  title: string;
  description: string;
  thumbnails: Record<string, { url: string; width?: number; height?: number }>;
  subscriber_count: number | null;
  video_count: number | null;
  uploads_playlist_id: string | null;
}

export interface PlaylistItem {
  video_id: string;
  title: string;
  channel_id: string | null;
  channel_title: string | null;
  thumbnails: Record<string, { url: string; width?: number; height?: number }>;
  published_at: string | null;
  position: number | null;
}

export interface ChannelVideosPage {
  items: PlaylistItem[];
  next_page_token: string | null;
}

export interface SubscriptionRow {
  id: number;
  channel_id: string;
  channel_title: string;
  channel_thumbnail_url: string | null;
  source: "app" | "youtube";
  sync_status: SyncStatus;
  subscribed_at: number;
  visible: boolean;
}

export interface PlaylistSummary {
  id: number;
  youtube_playlist_id: string | null;
  title: string;
  description: string | null;
  is_own: boolean;
  source: "app" | "youtube";
  sync_status: SyncStatus;
  video_count: number;
  created_at: number;
  updated_at: number;
  /**
   * `true` when this playlist is reachable through the child's
   * allowlist. Always true for `is_own=true`. For inbound
   * `source='youtube'` library imports the flag is computed by
   * joining against `allowlisted_playlists` server-side; child UIs
   * should hide rows where `visible === false`.
   */
  visible: boolean;
}

export interface PlaylistVideo {
  id: number;
  video_id: string;
  video_title: string;
  video_thumbnail_url: string | null;
  channel_title: string | null;
  position: number;
  added_at: number;
}

export interface PlaylistDetail extends PlaylistSummary {
  videos: PlaylistVideo[];
}

export interface LikeRow {
  id: number;
  video_id: string;
  video_title: string | null;
  video_thumbnail_url: string | null;
  source: "app" | "youtube";
  sync_status: SyncStatus;
  liked_at: number;
  /**
   * `true` when the liked video is reachable through the child's
   * allowlist (direct video allowlist; `video_likes` doesn't carry
   * channel/playlist metadata). Inbound YouTube-sourced likes that
   * the parent hasn't allowlisted come back with `visible: false` so
   * the child UI can drop them.
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

export interface Bookmark {
  id: number;
  video_id: string;
  video_title: string | null;
  timestamp_seconds: number;
  label: string | null;
  created_at: number;
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

export type NotificationType =
  | "time_limit_approaching"
  | "time_limit_reached"
  | "ytdlp_failure"
  | "sync_error"
  | "token_expired"
  | "new_search_term"
  | "system_update";

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
  subscriber_count: number | null;
  video_count: number | null;
  uploads_playlist_id: string | null;
  videos: PlaylistItem[];
  next_page_token: string | null;
}

export interface PlaylistPreview {
  id: string;
  title: string;
  description: string;
  thumbnails: Record<string, { url: string; width?: number; height?: number }>;
  channel_id: string | null;
  channel_title: string | null;
  item_count: number | null;
  videos: PlaylistItem[];
  next_page_token: string | null;
}

// ---------------------------------------------------------------------------
// Phase 18 — family playlists
// ---------------------------------------------------------------------------

export interface FamilyPlaylistSummary {
  id: number;
  created_by: number;
  title: string;
  description: string | null;
  created_at: number;
  updated_at: number;
  video_count: number;
}

export interface FamilyPlaylistVideo {
  id: number;
  video_id: string;
  video_title: string;
  video_thumbnail_url: string | null;
  channel_title: string | null;
  position: number;
  added_at: number;
}

export interface FamilyPlaylistDetail extends FamilyPlaylistSummary {
  videos: FamilyPlaylistVideo[];
  child_ids: number[];
}

// ---------------------------------------------------------------------------
// Phase 16 — caption track listing
// ---------------------------------------------------------------------------

export interface CaptionTrack {
  lang: string;
  name: string | null;
  auto_generated: boolean;
}
