//! Parent-only admin routes for the channel-backfill subsystem.
//!
//! Mirrors the existing `/api/admin/feed-refresher/{settings,capacity}`
//! shape so the parent settings page can host a sibling component
//! (`<hometube-channel-backfill-settings>`) with the same look + feel.
//!
//! Per-channel state comes from the `channel_sync_state` table; the
//! diagnostics-friendly subset is surfaced via the consolidated
//! `/api/admin/channel-sync-state` endpoint owned by [`crate::routes::feed`].
//! That endpoint already joins live + archived counts from
//! `channel_videos`, so this module only needs to expose the
//! backfill-specific settings + per-channel actions (run-now, unshelve).

use axum::{
    extract::{Path, State},
    http::StatusCode,
    Json,
};
use serde::{Deserialize, Serialize};

use crate::error::{AppError, AppResult};
use crate::services::channel_backfill::{self, BackfillConfig, KEY_ENABLED, KEY_IDLE_TICK_S,
    KEY_MAX_CONSECUTIVE_ERRORS_BEFORE_SHELVE, KEY_MIN_GAP_BETWEEN_CHANNELS_S,
    KEY_NOTIFY_ON_SHELVE, KEY_RE_BACKFILL_INTERVAL_S, KEY_SUBPROCESS_TIMEOUT_S,
    KEY_YTDLP_MAX_SLEEP_INTERVAL_S, KEY_YTDLP_SLEEP_INTERVAL_S,
    KEY_YTDLP_SLEEP_REQUESTS_S, RANGE_IDLE_TICK_S, RANGE_MAX_CONSECUTIVE_ERRORS,
    RANGE_MIN_GAP_BETWEEN_CHANNELS_S, RANGE_RE_BACKFILL_INTERVAL_S,
    RANGE_SUBPROCESS_TIMEOUT_S, RANGE_YTDLP_MAX_SLEEP_S, RANGE_YTDLP_SLEEP_S};
use crate::services::setup::set_config_value;
use crate::state::AppState;

#[derive(Debug, Serialize)]
pub struct BackfillSettings {
    pub enabled: bool,
    pub min_gap_between_channels_s: u64,
    pub re_backfill_interval_s: u64,
    pub subprocess_timeout_s: u64,
    pub ytdlp_sleep_requests_s: u32,
    pub ytdlp_sleep_interval_s: u32,
    pub ytdlp_max_sleep_interval_s: u32,
    pub max_consecutive_errors_before_shelve: i64,
    pub notify_on_shelve: bool,
    pub idle_tick_s: u64,
    pub raw: BackfillSettingsRaw,
}

#[derive(Debug, Serialize)]
pub struct BackfillSettingsRaw {
    pub enabled: Option<String>,
    pub min_gap_between_channels_s: Option<String>,
    pub re_backfill_interval_s: Option<String>,
    pub subprocess_timeout_s: Option<String>,
    pub ytdlp_sleep_requests_s: Option<String>,
    pub ytdlp_sleep_interval_s: Option<String>,
    pub ytdlp_max_sleep_interval_s: Option<String>,
    pub max_consecutive_errors_before_shelve: Option<String>,
    pub notify_on_shelve: Option<String>,
    pub idle_tick_s: Option<String>,
}

/// `GET /api/admin/channel-backfill/settings` — parent-only.
///
/// Returns the live tunables alongside the raw `app_config` strings,
/// matching the shape of the feed-refresher settings endpoint.
pub async fn admin_get_settings(
    State(state): State<AppState>,
) -> AppResult<Json<BackfillSettings>> {
    let (cfg, raw) = BackfillConfig::load_with_raw(&state.db).await;
    Ok(Json(BackfillSettings {
        enabled: cfg.enabled,
        min_gap_between_channels_s: cfg.min_gap_between_channels.as_secs(),
        re_backfill_interval_s: cfg.re_backfill_interval.as_secs(),
        subprocess_timeout_s: cfg.subprocess_timeout.as_secs(),
        ytdlp_sleep_requests_s: cfg.ytdlp_sleep_requests_s,
        ytdlp_sleep_interval_s: cfg.ytdlp_sleep_interval_s,
        ytdlp_max_sleep_interval_s: cfg.ytdlp_max_sleep_interval_s,
        max_consecutive_errors_before_shelve: cfg.max_consecutive_errors_before_shelve,
        notify_on_shelve: cfg.notify_on_shelve,
        idle_tick_s: cfg.idle_tick.as_secs(),
        raw: BackfillSettingsRaw {
            enabled: raw.enabled,
            min_gap_between_channels_s: raw.min_gap_between_channels_s,
            re_backfill_interval_s: raw.re_backfill_interval_s,
            subprocess_timeout_s: raw.subprocess_timeout_s,
            ytdlp_sleep_requests_s: raw.ytdlp_sleep_requests_s,
            ytdlp_sleep_interval_s: raw.ytdlp_sleep_interval_s,
            ytdlp_max_sleep_interval_s: raw.ytdlp_max_sleep_interval_s,
            max_consecutive_errors_before_shelve: raw.max_consecutive_errors_before_shelve,
            notify_on_shelve: raw.notify_on_shelve,
            idle_tick_s: raw.idle_tick_s,
        },
    }))
}

#[derive(Debug, Deserialize)]
pub struct UpdateBackfillSettings {
    #[serde(default)]
    pub enabled: Option<bool>,
    #[serde(default)]
    pub min_gap_between_channels_s: Option<u64>,
    #[serde(default)]
    pub re_backfill_interval_s: Option<u64>,
    #[serde(default)]
    pub subprocess_timeout_s: Option<u64>,
    #[serde(default)]
    pub ytdlp_sleep_requests_s: Option<u32>,
    #[serde(default)]
    pub ytdlp_sleep_interval_s: Option<u32>,
    #[serde(default)]
    pub ytdlp_max_sleep_interval_s: Option<u32>,
    #[serde(default)]
    pub max_consecutive_errors_before_shelve: Option<i64>,
    #[serde(default)]
    pub notify_on_shelve: Option<bool>,
    #[serde(default)]
    pub idle_tick_s: Option<u64>,
}

/// `PUT /api/admin/channel-backfill/settings` — parent-only.
pub async fn admin_put_settings(
    State(state): State<AppState>,
    Json(body): Json<UpdateBackfillSettings>,
) -> AppResult<Json<BackfillSettings>> {
    if let Some(v) = body.enabled {
        set_config_value(&state.db, KEY_ENABLED, &v.to_string()).await?;
    }
    if let Some(v) = body.min_gap_between_channels_s {
        if !RANGE_MIN_GAP_BETWEEN_CHANNELS_S.contains(&v) {
            return Err(AppError::BadRequest(format!(
                "min_gap_between_channels_s out of range [{}..={}]",
                RANGE_MIN_GAP_BETWEEN_CHANNELS_S.start(),
                RANGE_MIN_GAP_BETWEEN_CHANNELS_S.end()
            )));
        }
        set_config_value(&state.db, KEY_MIN_GAP_BETWEEN_CHANNELS_S, &v.to_string()).await?;
    }
    if let Some(v) = body.re_backfill_interval_s {
        if !RANGE_RE_BACKFILL_INTERVAL_S.contains(&v) {
            return Err(AppError::BadRequest(format!(
                "re_backfill_interval_s out of range [{}..={}]",
                RANGE_RE_BACKFILL_INTERVAL_S.start(),
                RANGE_RE_BACKFILL_INTERVAL_S.end()
            )));
        }
        set_config_value(&state.db, KEY_RE_BACKFILL_INTERVAL_S, &v.to_string()).await?;
    }
    if let Some(v) = body.subprocess_timeout_s {
        if !RANGE_SUBPROCESS_TIMEOUT_S.contains(&v) {
            return Err(AppError::BadRequest(format!(
                "subprocess_timeout_s out of range [{}..={}]",
                RANGE_SUBPROCESS_TIMEOUT_S.start(),
                RANGE_SUBPROCESS_TIMEOUT_S.end()
            )));
        }
        set_config_value(&state.db, KEY_SUBPROCESS_TIMEOUT_S, &v.to_string()).await?;
    }
    if let Some(v) = body.ytdlp_sleep_requests_s {
        if !RANGE_YTDLP_SLEEP_S.contains(&v) {
            return Err(AppError::BadRequest(format!(
                "ytdlp_sleep_requests_s out of range [{}..={}]",
                RANGE_YTDLP_SLEEP_S.start(),
                RANGE_YTDLP_SLEEP_S.end()
            )));
        }
        set_config_value(&state.db, KEY_YTDLP_SLEEP_REQUESTS_S, &v.to_string()).await?;
    }
    if let Some(v) = body.ytdlp_sleep_interval_s {
        if !RANGE_YTDLP_SLEEP_S.contains(&v) {
            return Err(AppError::BadRequest(format!(
                "ytdlp_sleep_interval_s out of range [{}..={}]",
                RANGE_YTDLP_SLEEP_S.start(),
                RANGE_YTDLP_SLEEP_S.end()
            )));
        }
        set_config_value(&state.db, KEY_YTDLP_SLEEP_INTERVAL_S, &v.to_string()).await?;
    }
    if let Some(v) = body.ytdlp_max_sleep_interval_s {
        if !RANGE_YTDLP_MAX_SLEEP_S.contains(&v) {
            return Err(AppError::BadRequest(format!(
                "ytdlp_max_sleep_interval_s out of range [{}..={}]",
                RANGE_YTDLP_MAX_SLEEP_S.start(),
                RANGE_YTDLP_MAX_SLEEP_S.end()
            )));
        }
        set_config_value(&state.db, KEY_YTDLP_MAX_SLEEP_INTERVAL_S, &v.to_string()).await?;
    }
    if let Some(v) = body.max_consecutive_errors_before_shelve {
        if !RANGE_MAX_CONSECUTIVE_ERRORS.contains(&v) {
            return Err(AppError::BadRequest(format!(
                "max_consecutive_errors_before_shelve out of range [{}..={}]",
                RANGE_MAX_CONSECUTIVE_ERRORS.start(),
                RANGE_MAX_CONSECUTIVE_ERRORS.end()
            )));
        }
        set_config_value(
            &state.db,
            KEY_MAX_CONSECUTIVE_ERRORS_BEFORE_SHELVE,
            &v.to_string(),
        )
        .await?;
    }
    if let Some(v) = body.notify_on_shelve {
        set_config_value(&state.db, KEY_NOTIFY_ON_SHELVE, &v.to_string()).await?;
    }
    if let Some(v) = body.idle_tick_s {
        if !RANGE_IDLE_TICK_S.contains(&v) {
            return Err(AppError::BadRequest(format!(
                "idle_tick_s out of range [{}..={}]",
                RANGE_IDLE_TICK_S.start(),
                RANGE_IDLE_TICK_S.end()
            )));
        }
        set_config_value(&state.db, KEY_IDLE_TICK_S, &v.to_string()).await?;
    }
    admin_get_settings(State(state)).await
}

/// `POST /api/admin/channel-backfill/run-now/:channelId` — parent-only.
pub async fn admin_run_now(
    State(state): State<AppState>,
    Path(channel_id): Path<String>,
) -> AppResult<StatusCode> {
    let n = channel_backfill::enqueue_run_now(&state.db, &channel_id).await?;
    if n == 0 {
        return Err(AppError::NotFound);
    }
    Ok(StatusCode::NO_CONTENT)
}

/// `POST /api/admin/channel-backfill/unshelve/:channelId` — parent-only.
pub async fn admin_unshelve(
    State(state): State<AppState>,
    Path(channel_id): Path<String>,
) -> AppResult<StatusCode> {
    let n = channel_backfill::unshelve(&state.db, &channel_id).await?;
    if n == 0 {
        return Err(AppError::NotFound);
    }
    Ok(StatusCode::NO_CONTENT)
}
