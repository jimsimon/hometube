/**
 * Shared types for HomeTube frontend components.
 *
 * Mirrors the JSON shapes returned by the backend API. Kept deliberately
 * narrow — only the fields the UI actually consumes.
 */

export type AccountType = 'parent' | 'child';

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
  account_type: 'parent' | 'child';
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
  max_quality: '480p' | '720p' | '1080p' | null;
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
  kind: 'channel' | 'playlist' | 'video';
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
  source_kind: 'channel' | 'playlist';
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

export interface UsageLimitResponse {
  reason: 'limit_exceeded' | 'outside_window';
  remaining_seconds: number;
}

/** Best-effort thumbnail-pick helper used across components. */
export function pickThumbnail(
  thumbs: Record<string, { url: string }>,
): string | null {
  for (const key of ['maxres', 'high', 'standard', 'medium', 'default']) {
    const t = thumbs[key];
    if (t) return t.url;
  }
  return null;
}
