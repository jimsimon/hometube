//! Watch-activity dashboard routes (parent-only).
//!
//! All four endpoints take `:id` (a child account id) and return data
//! aggregated from the `usage_log`, `watch_history`, and `search_log`
//! tables. The cron `youtube_sync` keeps `watch_history` populated for
//! anything the child has actually played; this module only reads.
//!
//! Routes:
//! - `GET /api/children/:id/activity/summary?period=day|week|month`
//! - `GET /api/children/:id/activity/history?limit=&before=`
//! - `GET /api/children/:id/activity/top-channels?period=week|month`
//! - `GET /api/children/:id/activity/search-log?limit=&before=`

use axum::{
    extract::{Path, Query, State},
    Json,
};
use chrono::Utc;
use serde::{Deserialize, Serialize};

use crate::error::{AppError, AppResult};
use crate::state::AppState;

/// Default page size for paginated history endpoints.
const DEFAULT_LIMIT: i64 = 50;
/// Hard cap so callers can't request the whole table.
const MAX_LIMIT: i64 = 200;

#[derive(Debug, Deserialize)]
pub struct PeriodQuery {
    /// One of `day`, `week`, `month`. Defaults to `week`.
    #[serde(default)]
    pub period: Option<String>,
}

fn period_seconds(period: Option<&str>) -> i64 {
    match period.unwrap_or("week") {
        "day" => 24 * 60 * 60,
        "month" => 30 * 24 * 60 * 60,
        // "week" + anything else
        _ => 7 * 24 * 60 * 60,
    }
}

#[derive(Debug, Serialize)]
pub struct ActivitySummary {
    pub period: String,
    pub total_seconds: i64,
    pub videos_watched: i64,
    pub sessions: i64,
    /// Daily totals over the last 30 days (oldest → newest).
    pub daily_minutes: Vec<DailyMinutes>,
}

#[derive(Debug, Serialize)]
pub struct DailyMinutes {
    pub date: String,
    pub minutes: i64,
}

/// `GET /api/children/:id/activity/summary?period=day|week|month`.
pub async fn summary(
    State(state): State<AppState>,
    Path(child_id): Path<i64>,
    Query(q): Query<PeriodQuery>,
) -> AppResult<Json<ActivitySummary>> {
    let period = q.period.clone().unwrap_or_else(|| "week".to_string());
    let window = period_seconds(Some(&period));
    let now = Utc::now().timestamp();
    let since = now - window;

    let (total_seconds, sessions): (Option<i64>, i64) = sqlx::query_as(
        "SELECT COALESCE(SUM(COALESCE(duration_seconds, 0)), 0), COUNT(*) \
         FROM usage_log \
         WHERE child_account_id = ? AND started_at >= ?",
    )
    .bind(child_id)
    .bind(since)
    .fetch_one(&state.db)
    .await?;

    let videos_watched: i64 = sqlx::query_scalar(
        "SELECT COUNT(DISTINCT video_id) FROM usage_log \
         WHERE child_account_id = ? AND started_at >= ?",
    )
    .bind(child_id)
    .bind(since)
    .fetch_one(&state.db)
    .await
    .unwrap_or(0);

    // Daily totals for the last 30 days.
    let thirty_days_ago = now - 30 * 24 * 60 * 60;
    let rows: Vec<(String, i64)> = sqlx::query_as(
        "SELECT strftime('%Y-%m-%d', started_at, 'unixepoch') as day, \
                COALESCE(SUM(COALESCE(duration_seconds, 0)) / 60, 0) as minutes \
         FROM usage_log \
         WHERE child_account_id = ? AND started_at >= ? \
         GROUP BY day \
         ORDER BY day ASC",
    )
    .bind(child_id)
    .bind(thirty_days_ago)
    .fetch_all(&state.db)
    .await
    .unwrap_or_default();

    let daily_minutes = rows
        .into_iter()
        .map(|(date, minutes)| DailyMinutes { date, minutes })
        .collect();

    Ok(Json(ActivitySummary {
        period,
        total_seconds: total_seconds.unwrap_or(0),
        videos_watched,
        sessions,
        daily_minutes,
    }))
}

#[derive(Debug, Deserialize)]
pub struct PageQuery {
    #[serde(default)]
    pub limit: Option<i64>,
    /// Pagination cursor — return items strictly older than this unix ts.
    #[serde(default)]
    pub before: Option<i64>,
}

#[derive(Debug, Serialize, sqlx::FromRow)]
pub struct HistoryEntry {
    pub video_id: String,
    pub video_title: Option<String>,
    pub video_thumbnail_url: Option<String>,
    pub channel_title: Option<String>,
    pub started_at: i64,
    pub ended_at: Option<i64>,
    pub duration_seconds: Option<i64>,
}

/// `GET /api/children/:id/activity/history`.
pub async fn history(
    State(state): State<AppState>,
    Path(child_id): Path<i64>,
    Query(q): Query<PageQuery>,
) -> AppResult<Json<Vec<HistoryEntry>>> {
    let limit = q.limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT);
    let before = q.before.unwrap_or(i64::MAX);

    let rows: Vec<HistoryEntry> = sqlx::query_as(
        "SELECT u.video_id, \
                w.video_title as video_title, \
                w.video_thumbnail_url as video_thumbnail_url, \
                w.channel_title as channel_title, \
                u.started_at, \
                u.ended_at, \
                u.duration_seconds \
         FROM usage_log u \
         LEFT JOIN watch_history w \
                ON w.child_account_id = u.child_account_id AND w.video_id = u.video_id \
         WHERE u.child_account_id = ? AND u.started_at < ? \
         ORDER BY u.started_at DESC \
         LIMIT ?",
    )
    .bind(child_id)
    .bind(before)
    .bind(limit)
    .fetch_all(&state.db)
    .await?;

    Ok(Json(rows))
}

#[derive(Debug, Serialize, sqlx::FromRow)]
pub struct TopChannel {
    pub channel_title: Option<String>,
    pub total_seconds: i64,
    pub videos_watched: i64,
}

/// `GET /api/children/:id/activity/top-channels?period=week|month`.
pub async fn top_channels(
    State(state): State<AppState>,
    Path(child_id): Path<i64>,
    Query(q): Query<PeriodQuery>,
) -> AppResult<Json<Vec<TopChannel>>> {
    let period = q.period.clone().unwrap_or_else(|| "week".to_string());
    let window = period_seconds(Some(&period));
    let since = Utc::now().timestamp() - window;

    let rows: Vec<TopChannel> = sqlx::query_as(
        "SELECT w.channel_title as channel_title, \
                COALESCE(SUM(COALESCE(u.duration_seconds, 0)), 0) as total_seconds, \
                COUNT(DISTINCT u.video_id) as videos_watched \
         FROM usage_log u \
         LEFT JOIN watch_history w \
                ON w.child_account_id = u.child_account_id AND w.video_id = u.video_id \
         WHERE u.child_account_id = ? AND u.started_at >= ? \
         GROUP BY w.channel_title \
         ORDER BY total_seconds DESC \
         LIMIT 20",
    )
    .bind(child_id)
    .bind(since)
    .fetch_all(&state.db)
    .await
    .unwrap_or_default();

    Ok(Json(rows))
}

#[derive(Debug, Serialize, sqlx::FromRow)]
pub struct SearchLogEntry {
    pub id: i64,
    pub query: String,
    pub result_count: i64,
    pub searched_at: i64,
}

/// `GET /api/children/:id/activity/search-log`.
pub async fn search_log(
    State(state): State<AppState>,
    Path(child_id): Path<i64>,
    Query(q): Query<PageQuery>,
) -> AppResult<Json<Vec<SearchLogEntry>>> {
    let limit = q.limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT);
    let before = q.before.unwrap_or(i64::MAX);

    let rows: Vec<SearchLogEntry> = sqlx::query_as(
        "SELECT id, query, result_count, searched_at \
         FROM search_log \
         WHERE child_account_id = ? AND searched_at < ? \
         ORDER BY searched_at DESC \
         LIMIT ?",
    )
    .bind(child_id)
    .bind(before)
    .bind(limit)
    .fetch_all(&state.db)
    .await?;

    Ok(Json(rows))
}

/// Verify that `child_id` actually refers to a child account. Helpers
/// can call this to fail fast on bad input.
#[allow(dead_code)]
pub(crate) async fn ensure_child_exists(state: &AppState, child_id: i64) -> AppResult<()> {
    let count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM accounts WHERE id = ? AND account_type = 'child'")
            .bind(child_id)
            .fetch_one(&state.db)
            .await?;
    if count == 0 {
        return Err(AppError::NotFound);
    }
    Ok(())
}
