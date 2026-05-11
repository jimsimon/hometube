//! Usage-limit enforcement.
//!
//! Wrapped around every child-facing video endpoint (`/api/videos/*`
//! and `/api/proxy/*`), this middleware checks two conditions before
//! letting the request through:
//!
//! 1. **Daily-cap**: total `usage_log.duration_seconds` for today must
//!    be below `usage_limits.max_hours` for the current day-of-week.
//! 2. **Allowed window**: the current local time must be within
//!    `usage_limits.allowed_start_time` ..= `allowed_end_time`.
//!
//! On violation it returns `403` with a JSON body:
//!
//! ```json
//! { "reason": "limit_exceeded" | "outside_window", "remaining_seconds": 0 }
//! ```
//!
//! When today's remaining time drops below 15 minutes (and we haven't
//! already warned today) we insert a `time_limit_approaching`
//! notification for every parent. Phase 17's notification dispatcher
//! turns the row into a real notification on the next dashboard load.

use axum::{
    extract::State,
    http::{Request, StatusCode},
    middleware::Next,
    response::{IntoResponse, Response},
    Json,
};
use chrono::{Datelike, Local, Timelike};
use serde::Serialize;
use sqlx::SqlitePool;
use tracing::warn;

use crate::error::AppResult;
use crate::middleware::auth::CurrentAccount;
use crate::models::account::AccountType;
use crate::services::notifications::{self, TYPE_TIME_LIMIT_APPROACHING, TYPE_TIME_LIMIT_REACHED};
use crate::state::AppState;

/// Threshold below which we emit a `time_limit_approaching`
/// notification.
const APPROACHING_SECONDS: i64 = 15 * 60;

#[derive(Debug, Serialize)]
struct LimitResponse {
    reason: &'static str,
    remaining_seconds: i64,
}

pub async fn enforce_usage_limit(
    State(state): State<AppState>,
    req: Request<axum::body::Body>,
    next: Next,
) -> Response {
    let current = match req.extensions().get::<CurrentAccount>().cloned() {
        Some(c) => c,
        None => return next.run(req).await,
    };
    if !matches!(current.account_type, AccountType::Child) {
        return next.run(req).await;
    }

    let now = Local::now();
    let day_of_week = now.weekday().num_days_from_sunday() as i64;
    let now_minutes = now.hour() as i64 * 60 + now.minute() as i64;

    let limit_row: Option<(f64, String, String)> = match sqlx::query_as(
        "SELECT max_hours, allowed_start_time, allowed_end_time FROM usage_limits \
         WHERE child_account_id = ? AND day_of_week = ?",
    )
    .bind(current.id)
    .bind(day_of_week)
    .fetch_optional(&state.db)
    .await
    {
        Ok(r) => r,
        Err(err) => {
            warn!(error = %err, "fetching usage limits");
            return next.run(req).await;
        }
    };

    if let Some((max_hours, start, end)) = limit_row {
        let max_seconds = (max_hours * 3600.0) as i64;

        if let (Some(start_m), Some(end_m)) = (parse_hhmm(&start), parse_hhmm(&end)) {
            if now_minutes < start_m || now_minutes >= end_m {
                return (
                    StatusCode::FORBIDDEN,
                    Json(LimitResponse {
                        reason: "outside_window",
                        remaining_seconds: 0,
                    }),
                )
                    .into_response();
            }
        }

        let used_today: i64 = sqlx::query_scalar(
            "SELECT COALESCE(SUM(duration_seconds), 0) FROM usage_log \
             WHERE child_account_id = ? AND started_at >= unixepoch() - 86400",
        )
        .bind(current.id)
        .fetch_one(&state.db)
        .await
        .unwrap_or(0);
        let remaining = max_seconds - used_today;

        if remaining <= 0 {
            // Best-effort daily-limit notification.
            let _ = notify_parents_limit_reached(&state.db, current.id).await;
            return (
                StatusCode::FORBIDDEN,
                Json(LimitResponse {
                    reason: "limit_exceeded",
                    remaining_seconds: 0,
                }),
            )
                .into_response();
        }

        if remaining < APPROACHING_SECONDS {
            // Best-effort notification — never block the request on it.
            let _ = notify_parents(&state.db, current.id, remaining).await;
        }
    }

    next.run(req).await
}

fn parse_hhmm(s: &str) -> Option<i64> {
    let bytes = s.as_bytes();
    if bytes.len() != 5 || bytes[2] != b':' {
        return None;
    }
    let hh: i64 = s[..2].parse().ok()?;
    let mm: i64 = s[3..].parse().ok()?;
    Some(hh * 60 + mm)
}

/// Insert a `time_limit_reached` notification for every parent unless
/// one already exists today. Delegates dedup + insert to the
/// notifications dispatcher.
async fn notify_parents_limit_reached(pool: &SqlitePool, child_id: i64) -> AppResult<()> {
    let metadata = serde_json::json!({ "child_account_id": child_id });
    let key = notifications::json_fragment_key("child_account_id", &child_id);
    notifications::broadcast_once_per_day(
        pool,
        TYPE_TIME_LIMIT_REACHED,
        &key,
        "Daily limit reached",
        "Watch time used up for today.",
        &metadata,
    )
    .await
}

async fn notify_parents(pool: &SqlitePool, child_id: i64, remaining: i64) -> AppResult<()> {
    let metadata = serde_json::json!({
        "child_account_id": child_id,
        "remaining_seconds": remaining,
    });
    let key = notifications::json_fragment_key("child_account_id", &child_id);
    let message = format!(
        "Less than {} minutes remain today.",
        (remaining / 60).max(1)
    );
    notifications::broadcast_once_per_day(
        pool,
        TYPE_TIME_LIMIT_APPROACHING,
        &key,
        "Daily limit approaching",
        &message,
        &metadata,
    )
    .await
}
