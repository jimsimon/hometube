//! Per-child settings (parent only).
//!
//! Knobs for the player (max quality, autoplay, speed lock, downloads
//! on/off, Chromecast toggle).

use axum::{
    extract::{Path, State},
    Json,
};
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
    /// Whether the player should load the Chromecast SDK and surface a
    /// cast button. Gated per-child so parents can opt children in
    /// individually; defaults OFF (see migration 011).
    pub chromecast_enabled: i64,
}

impl Serialize for ChildSettings {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        use serde::ser::SerializeStruct;
        let mut s = serializer.serialize_struct("ChildSettings", 7)?;
        s.serialize_field("child_account_id", &self.child_account_id)?;
        s.serialize_field("downloads_enabled", &(self.downloads_enabled != 0))?;
        s.serialize_field("max_quality", &self.max_quality)?;
        s.serialize_field("playback_speed_locked", &(self.playback_speed_locked != 0))?;
        s.serialize_field("autoplay_enabled", &(self.autoplay_enabled != 0))?;
        s.serialize_field("autoplay_max_consecutive", &self.autoplay_max_consecutive)?;
        s.serialize_field("chromecast_enabled", &(self.chromecast_enabled != 0))?;
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
    #[serde(default)]
    pub chromecast_enabled: Option<bool>,
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
                autoplay_enabled, autoplay_max_consecutive, chromecast_enabled \
         FROM child_settings WHERE child_account_id = ?",
    )
    .bind(child_id)
    .fetch_one(&state.db)
    .await?;
    Ok(Json(row))
}

/// `GET /api/children/me/settings` — read-only settings view for the
/// **currently signed-in child**. Mirrors [`get_settings`] but uses the
/// session account rather than a path parameter, and never accepts
/// mutations (parents must use `PUT /api/children/:id/settings`).
///
/// The route is gated by `require_child` middleware in
/// [`crate::routes::router`], so we don't re-check the role here.
pub async fn get_my_settings(
    State(state): State<AppState>,
    current: crate::middleware::auth::CurrentAccount,
) -> AppResult<Json<ChildSettings>> {
    ensure_settings_row(&state, current.id).await?;
    let row: ChildSettings = sqlx::query_as(
        "SELECT child_account_id, downloads_enabled, max_quality, playback_speed_locked, \
                autoplay_enabled, autoplay_max_consecutive, chromecast_enabled \
         FROM child_settings WHERE child_account_id = ?",
    )
    .bind(current.id)
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
    if let Some(v) = body.chromecast_enabled {
        sqlx::query(
            "UPDATE child_settings SET chromecast_enabled = ?, updated_at = unixepoch() \
             WHERE child_account_id = ?",
        )
        .bind(v as i64)
        .bind(child_id)
        .execute(&state.db)
        .await?;
    }

    get_settings(State(state), Path(child_id)).await
}

async fn ensure_settings_row(state: &AppState, child_id: i64) -> AppResult<()> {
    sqlx::query("INSERT OR IGNORE INTO child_settings (child_account_id) VALUES (?)")
        .bind(child_id)
        .execute(&state.db)
        .await?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

async fn require_child(state: &AppState, child_id: i64) -> AppResult<()> {
    if !access::is_child_account(&state.db, child_id).await? {
        return Err(AppError::BadRequest("target account is not a child".into()));
    }
    Ok(())
}
