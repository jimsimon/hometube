//! Sleep timer routes (child-only).
//!
//! A child can ask HomeTube to stop playback after the current video, or
//! after N minutes. Only one timer is active per child.
//!
//! ## Active flag semantics
//!
//! The schema declares `is_active INTEGER NOT NULL DEFAULT 1` plus
//! `UNIQUE(child_account_id, is_active)`. Because the column is
//! `NOT NULL` and the unique constraint covers it directly, we can't
//! soft-deactivate by writing `0` (that would collide on the second
//! cancellation) or `NULL`. Instead, **HomeTube deletes superseded /
//! cancelled rows.** Only the active timer ever lives in the table;
//! historical analytics are out of scope for the sleep timer.

use axum::{extract::State, http::StatusCode, Json};
use serde::{Deserialize, Serialize};

use crate::error::{AppError, AppResult};
use crate::middleware::auth::CurrentAccount;
use crate::state::AppState;

#[derive(Debug, Serialize, sqlx::FromRow)]
pub struct SleepTimer {
    pub id: i64,
    pub timer_type: String,
    pub minutes_remaining: Option<i64>,
    pub videos_remaining: Option<i64>,
    pub started_at: i64,
    pub expires_at: Option<i64>,
}

/// `GET /api/timer`.
pub async fn get(
    State(state): State<AppState>,
    current: CurrentAccount,
) -> AppResult<Json<Option<SleepTimer>>> {
    let row: Option<SleepTimer> = sqlx::query_as(
        "SELECT id, timer_type, minutes_remaining, videos_remaining, started_at, expires_at \
         FROM sleep_timers \
         WHERE child_account_id = ? AND is_active = 1",
    )
    .bind(current.id)
    .fetch_optional(&state.db)
    .await?;
    Ok(Json(row))
}

#[derive(Debug, Deserialize)]
pub struct CreateBody {
    /// `"after_video"` or `"minutes"`.
    pub r#type: String,
    /// Required when `type == "minutes"`. Range: 1..=180.
    #[serde(default)]
    pub minutes: Option<i64>,
}

/// `POST /api/timer`.
pub async fn create(
    State(state): State<AppState>,
    current: CurrentAccount,
    Json(body): Json<CreateBody>,
) -> AppResult<Json<SleepTimer>> {
    let timer_type = match body.r#type.as_str() {
        "after_video" => "after_video",
        "minutes" => "minutes",
        _ => {
            return Err(AppError::BadRequest(
                "type must be 'after_video' or 'minutes'".into(),
            ))
        }
    };

    let mut tx = state.db.begin().await?;

    // Drop any existing timer for this child. The unique constraint
    // `UNIQUE(child_account_id, is_active)` plus `is_active NOT NULL`
    // means there's no value we can rewrite to "deactivate" safely;
    // deletion is the simplest, predictable approach.
    sqlx::query("DELETE FROM sleep_timers WHERE child_account_id = ?")
        .bind(current.id)
        .execute(&mut *tx)
        .await?;

    let (minutes_remaining, videos_remaining, expires_at) = if timer_type == "minutes" {
        let minutes = body
            .minutes
            .ok_or_else(|| AppError::BadRequest("minutes required for type='minutes'".into()))?;
        if !(1..=180).contains(&minutes) {
            return Err(AppError::BadRequest(
                "minutes must be between 1 and 180".into(),
            ));
        }
        let expires_at: i64 = sqlx::query_scalar("SELECT unixepoch() + ?")
            .bind(minutes * 60)
            .fetch_one(&mut *tx)
            .await?;
        (Some(minutes), None, Some(expires_at))
    } else {
        (None, Some(1), None)
    };

    let id: i64 = sqlx::query_scalar(
        "INSERT INTO sleep_timers \
            (child_account_id, timer_type, minutes_remaining, videos_remaining, expires_at, is_active) \
         VALUES (?, ?, ?, ?, ?, 1) \
         RETURNING id",
    )
    .bind(current.id)
    .bind(timer_type)
    .bind(minutes_remaining)
    .bind(videos_remaining)
    .bind(expires_at)
    .fetch_one(&mut *tx)
    .await?;

    tx.commit().await?;

    let row: SleepTimer = sqlx::query_as(
        "SELECT id, timer_type, minutes_remaining, videos_remaining, started_at, expires_at \
         FROM sleep_timers WHERE id = ?",
    )
    .bind(id)
    .fetch_one(&state.db)
    .await?;
    Ok(Json(row))
}

/// `DELETE /api/timer`.
pub async fn cancel(
    State(state): State<AppState>,
    current: CurrentAccount,
) -> AppResult<StatusCode> {
    sqlx::query("DELETE FROM sleep_timers WHERE child_account_id = ?")
        .bind(current.id)
        .execute(&state.db)
        .await?;
    Ok(StatusCode::NO_CONTENT)
}
