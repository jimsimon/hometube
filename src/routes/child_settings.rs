//! Per-child settings + usage limits + usage stats (parent only).
//!
//! Three concerns live in this module:
//!
//! 1. `child_settings` — knobs for the player (max quality, autoplay,
//!    speed lock, downloads on/off).
//! 2. `usage_limits` — seven rows per child, one per day-of-week.
//!    `PUT` replaces the entire set in a transaction.
//! 3. `usage-stats` — read-only aggregate over `usage_log`.

use axum::{
    extract::{Path, State},
    Json,
};
use chrono::{Datelike, Local};
use serde::{Deserialize, Serialize};

use crate::error::{AppError, AppResult};
use crate::services::access;
use crate::state::AppState;

// ---------------------------------------------------------------------------
// Child settings
// ---------------------------------------------------------------------------

/// JSON view of a row from `child_settings`. SQLite stores booleans as
/// `INTEGER` 0/1; we deserialise into `i64` to avoid sqlx's strict
/// type matching, then expose them as JSON booleans through a
/// hand-written [`Serialize`] impl.
#[derive(Debug, sqlx::FromRow)]
pub struct ChildSettings {
    pub child_account_id: i64,
    pub downloads_enabled: i64,
    pub max_quality: Option<String>,
    pub playback_speed_locked: i64,
    pub autoplay_enabled: i64,
    pub autoplay_max_consecutive: Option<i64>,
}

impl Serialize for ChildSettings {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        use serde::ser::SerializeStruct;
        let mut s = serializer.serialize_struct("ChildSettings", 6)?;
        s.serialize_field("child_account_id", &self.child_account_id)?;
        s.serialize_field("downloads_enabled", &(self.downloads_enabled != 0))?;
        s.serialize_field("max_quality", &self.max_quality)?;
        s.serialize_field("playback_speed_locked", &(self.playback_speed_locked != 0))?;
        s.serialize_field("autoplay_enabled", &(self.autoplay_enabled != 0))?;
        s.serialize_field("autoplay_max_consecutive", &self.autoplay_max_consecutive)?;
        s.end()
    }
}

#[derive(Debug, Deserialize, Default)]
pub struct UpdateSettingsBody {
    #[serde(default)]
    pub downloads_enabled: Option<bool>,
    #[serde(default)]
    pub max_quality: Option<Option<String>>,
    #[serde(default)]
    pub playback_speed_locked: Option<bool>,
    #[serde(default)]
    pub autoplay_enabled: Option<bool>,
    #[serde(default)]
    pub autoplay_max_consecutive: Option<Option<i64>>,
}

/// `GET /api/children/:id/settings`.
pub async fn get_settings(
    State(state): State<AppState>,
    Path(child_id): Path<i64>,
) -> AppResult<Json<ChildSettings>> {
    require_child(&state, child_id).await?;
    ensure_settings_row(&state, child_id).await?;
    let row: ChildSettings = sqlx::query_as(
        "SELECT child_account_id, downloads_enabled, max_quality, playback_speed_locked, \
                autoplay_enabled, autoplay_max_consecutive \
         FROM child_settings WHERE child_account_id = ?",
    )
    .bind(child_id)
    .fetch_one(&state.db)
    .await?;
    Ok(Json(row))
}

/// `PUT /api/children/:id/settings`.
pub async fn update_settings(
    State(state): State<AppState>,
    Path(child_id): Path<i64>,
    Json(body): Json<UpdateSettingsBody>,
) -> AppResult<Json<ChildSettings>> {
    require_child(&state, child_id).await?;
    ensure_settings_row(&state, child_id).await?;

    if let Some(v) = body.downloads_enabled {
        sqlx::query(
            "UPDATE child_settings SET downloads_enabled = ?, updated_at = unixepoch() \
             WHERE child_account_id = ?",
        )
        .bind(v as i64)
        .bind(child_id)
        .execute(&state.db)
        .await?;
    }
    if let Some(q) = body.max_quality {
        if let Some(ref label) = q {
            if !matches!(label.as_str(), "480p" | "720p" | "1080p") {
                return Err(AppError::BadRequest(
                    "max_quality must be 480p, 720p, or 1080p (or null)".into(),
                ));
            }
        }
        sqlx::query(
            "UPDATE child_settings SET max_quality = ?, updated_at = unixepoch() \
             WHERE child_account_id = ?",
        )
        .bind(q)
        .bind(child_id)
        .execute(&state.db)
        .await?;
    }
    if let Some(v) = body.playback_speed_locked {
        sqlx::query(
            "UPDATE child_settings SET playback_speed_locked = ?, updated_at = unixepoch() \
             WHERE child_account_id = ?",
        )
        .bind(v as i64)
        .bind(child_id)
        .execute(&state.db)
        .await?;
    }
    if let Some(v) = body.autoplay_enabled {
        sqlx::query(
            "UPDATE child_settings SET autoplay_enabled = ?, updated_at = unixepoch() \
             WHERE child_account_id = ?",
        )
        .bind(v as i64)
        .bind(child_id)
        .execute(&state.db)
        .await?;
    }
    if let Some(v) = body.autoplay_max_consecutive {
        sqlx::query(
            "UPDATE child_settings SET autoplay_max_consecutive = ?, updated_at = unixepoch() \
             WHERE child_account_id = ?",
        )
        .bind(v)
        .bind(child_id)
        .execute(&state.db)
        .await?;
    }

    get_settings(State(state), Path(child_id)).await
}

async fn ensure_settings_row(state: &AppState, child_id: i64) -> AppResult<()> {
    sqlx::query(
        "INSERT OR IGNORE INTO child_settings (child_account_id) VALUES (?)",
    )
    .bind(child_id)
    .execute(&state.db)
    .await?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Usage limits
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize, Deserialize, sqlx::FromRow)]
pub struct UsageLimit {
    pub day_of_week: i64,
    pub max_hours: f64,
    pub allowed_start_time: String,
    pub allowed_end_time: String,
}

/// `GET /api/children/:id/usage-limits`.
pub async fn get_limits(
    State(state): State<AppState>,
    Path(child_id): Path<i64>,
) -> AppResult<Json<Vec<UsageLimit>>> {
    require_child(&state, child_id).await?;
    let rows: Vec<UsageLimit> = sqlx::query_as(
        "SELECT day_of_week, max_hours, allowed_start_time, allowed_end_time \
         FROM usage_limits WHERE child_account_id = ? ORDER BY day_of_week",
    )
    .bind(child_id)
    .fetch_all(&state.db)
    .await?;
    Ok(Json(rows))
}

/// `PUT /api/children/:id/usage-limits` — replaces the full set in a
/// transaction. The body must contain rows for every day the parent
/// wants enforced; any day not present in the body is removed.
pub async fn update_limits(
    State(state): State<AppState>,
    Path(child_id): Path<i64>,
    Json(rows): Json<Vec<UsageLimit>>,
) -> AppResult<Json<Vec<UsageLimit>>> {
    require_child(&state, child_id).await?;

    // Validation pass.
    for row in &rows {
        if !(0..=6).contains(&row.day_of_week) {
            return Err(AppError::BadRequest(
                "day_of_week must be between 0 and 6".into(),
            ));
        }
        if row.max_hours < 0.0 || row.max_hours > 24.0 {
            return Err(AppError::BadRequest(
                "max_hours must be between 0 and 24".into(),
            ));
        }
        if !is_valid_hhmm(&row.allowed_start_time)
            || !is_valid_hhmm(&row.allowed_end_time)
        {
            return Err(AppError::BadRequest(
                "allowed_start_time and allowed_end_time must be HH:MM".into(),
            ));
        }
    }

    let mut tx = state.db.begin().await?;
    sqlx::query("DELETE FROM usage_limits WHERE child_account_id = ?")
        .bind(child_id)
        .execute(&mut *tx)
        .await?;
    for row in &rows {
        sqlx::query(
            "INSERT INTO usage_limits \
                (child_account_id, day_of_week, max_hours, allowed_start_time, allowed_end_time) \
             VALUES (?, ?, ?, ?, ?)",
        )
        .bind(child_id)
        .bind(row.day_of_week)
        .bind(row.max_hours)
        .bind(&row.allowed_start_time)
        .bind(&row.allowed_end_time)
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;

    get_limits(State(state), Path(child_id)).await
}

fn is_valid_hhmm(s: &str) -> bool {
    let bytes = s.as_bytes();
    if bytes.len() != 5 || bytes[2] != b':' {
        return false;
    }
    let hh: u32 = match s[..2].parse() {
        Ok(n) => n,
        Err(_) => return false,
    };
    let mm: u32 = match s[3..].parse() {
        Ok(n) => n,
        Err(_) => return false,
    };
    hh < 24 && mm < 60
}

// ---------------------------------------------------------------------------
// Usage stats
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize)]
pub struct UsageStats {
    /// Today's used time, in seconds.
    pub today_seconds: i64,
    /// Limit for today, in seconds (None means "no limit configured").
    pub today_limit_seconds: Option<i64>,
    /// Remaining seconds today (None means "no limit configured").
    pub today_remaining_seconds: Option<i64>,
    /// Per-day-of-week aggregate over the trailing seven days.
    pub weekly: Vec<DayUsage>,
}

#[derive(Debug, Serialize)]
pub struct DayUsage {
    pub date: String, // ISO 8601, YYYY-MM-DD
    pub day_of_week: i64,
    pub used_seconds: i64,
}

/// `GET /api/children/:id/usage-stats`.
pub async fn usage_stats(
    State(state): State<AppState>,
    Path(child_id): Path<i64>,
) -> AppResult<Json<UsageStats>> {
    require_child(&state, child_id).await?;

    // Today's used seconds — sum durations whose started_at is in the
    // last 24h. Using the local-time "today" requires a timezone we
    // don't have; we approximate with "last 24h" for Phase 4-6, which
    // matches what the heartbeat handler enforces.
    let today_seconds: i64 = sqlx::query_scalar(
        "SELECT COALESCE(SUM(duration_seconds), 0) \
         FROM usage_log \
         WHERE child_account_id = ? \
           AND started_at >= unixepoch() - 86400",
    )
    .bind(child_id)
    .fetch_one(&state.db)
    .await
    .unwrap_or(0);

    // Today's configured limit, expressed in seconds.
    let dow = Local::now().weekday().num_days_from_sunday() as i64;
    let max_hours: Option<f64> = sqlx::query_scalar(
        "SELECT max_hours FROM usage_limits \
         WHERE child_account_id = ? AND day_of_week = ?",
    )
    .bind(child_id)
    .bind(dow)
    .fetch_optional(&state.db)
    .await?;
    let today_limit_seconds = max_hours.map(|h| (h * 3600.0) as i64);
    let today_remaining_seconds =
        today_limit_seconds.map(|lim| (lim - today_seconds).max(0));

    // Weekly: walk the last seven days locally.
    let mut weekly = Vec::with_capacity(7);
    let today = Local::now().date_naive();
    for offset in 0..7 {
        let date = today - chrono::Duration::days(offset);
        let day_start = date
            .and_hms_opt(0, 0, 0)
            .and_then(|naive| naive.and_local_timezone(Local).single())
            .map(|d| d.timestamp())
            .unwrap_or(0);
        let day_end = day_start + 86400;
        let used: i64 = sqlx::query_scalar(
            "SELECT COALESCE(SUM(duration_seconds), 0) \
             FROM usage_log \
             WHERE child_account_id = ? \
               AND started_at >= ? AND started_at < ?",
        )
        .bind(child_id)
        .bind(day_start)
        .bind(day_end)
        .fetch_one(&state.db)
        .await
        .unwrap_or(0);
        weekly.push(DayUsage {
            date: date.format("%Y-%m-%d").to_string(),
            day_of_week: date.weekday().num_days_from_sunday() as i64,
            used_seconds: used,
        });
    }

    Ok(Json(UsageStats {
        today_seconds,
        today_limit_seconds,
        today_remaining_seconds,
        weekly,
    }))
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

async fn require_child(state: &AppState, child_id: i64) -> AppResult<()> {
    if !access::is_child_account(&state.db, child_id).await? {
        return Err(AppError::BadRequest(
            "target account is not a child".into(),
        ));
    }
    Ok(())
}
