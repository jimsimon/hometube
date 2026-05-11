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

use crate::middleware::auth::CurrentAccount;
use crate::models::account::AccountType;
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

async fn notify_parents(
    pool: &SqlitePool,
    child_id: i64,
    remaining: i64,
) -> Result<(), sqlx::Error> {
    let parents: Vec<(i64,)> =
        sqlx::query_as("SELECT id FROM accounts WHERE account_type = 'parent'")
            .fetch_all(pool)
            .await?;
    let metadata = serde_json::json!({
        "child_account_id": child_id,
        "remaining_seconds": remaining,
    });
    let metadata_str = metadata.to_string();
    for (parent_id,) in parents {
        // Skip if a notification already exists for today.
        let exists: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM parent_notifications \
             WHERE parent_account_id = ? \
               AND notification_type = 'time_limit_approaching' \
               AND created_at >= unixepoch() - 86400",
        )
        .bind(parent_id)
        .fetch_one(pool)
        .await
        .unwrap_or(0);
        if exists > 0 {
            continue;
        }
        sqlx::query(
            "INSERT INTO parent_notifications \
                (parent_account_id, notification_type, title, message, metadata) \
             VALUES (?, 'time_limit_approaching', ?, ?, ?)",
        )
        .bind(parent_id)
        .bind("Daily limit approaching")
        .bind(format!(
            "Less than {} minutes remain today.",
            (remaining / 60).max(1)
        ))
        .bind(&metadata_str)
        .execute(pool)
        .await?;
    }
    Ok(())
}
