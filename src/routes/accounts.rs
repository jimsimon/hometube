//! Account management routes (parent only).
//!
//! These endpoints back the family-management UI:
//!
//! - `GET /api/accounts` — list every account
//! - `GET /api/accounts/:id` — fetch one
//! - `PUT /api/accounts/:id` — change display name and/or account type
//! - `DELETE /api/accounts/:id` — remove an account (refuses to remove
//!   the last remaining parent)

use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    Json,
};
use serde::Deserialize;

use crate::error::{AppError, AppResult};
use crate::models::account::{self, AccountSummary, AccountType};
use crate::state::AppState;

#[derive(Debug, Deserialize)]
pub struct UpdateAccountBody {
    #[serde(default)]
    pub display_name: Option<String>,
    /// Optional new role: `"parent"` or `"child"`.
    #[serde(default)]
    pub account_type: Option<String>,
}

#[derive(Debug, Deserialize, Default)]
pub struct ListQuery {
    /// Filter to a single account type (`"parent"` or `"child"`). Used
    /// by the parent dashboard's child-selector dropdown.
    #[serde(default, rename = "type")]
    pub account_type: Option<String>,
}

/// `GET /api/accounts` — list every account, or only those matching the
/// optional `?type=parent|child` filter.
pub async fn list(
    State(state): State<AppState>,
    Query(q): Query<ListQuery>,
) -> AppResult<Json<Vec<AccountSummary>>> {
    let rows = account::list_all(&state.db).await?;
    let filtered: Vec<AccountSummary> = match q.account_type.as_deref() {
        Some(s) => {
            let want = AccountType::parse(s)
                .ok_or_else(|| AppError::BadRequest(format!("unknown account_type {s:?}")))?;
            rows.iter()
                .filter(|a| a.typed() == want)
                .map(AccountSummary::from)
                .collect()
        }
        None => rows.iter().map(AccountSummary::from).collect(),
    };
    Ok(Json(filtered))
}

/// `GET /api/accounts/:id` — fetch a single account.
pub async fn get(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> AppResult<Json<AccountSummary>> {
    let acct = account::find_by_id(&state.db, id)
        .await?
        .ok_or(AppError::NotFound)?;
    Ok(Json(AccountSummary::from(&acct)))
}

/// `PUT /api/accounts/:id` — update display name / account type.
///
/// Switching the *only* parent to a child would lock the system; the
/// handler refuses that case with 400.
pub async fn update(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    Json(body): Json<UpdateAccountBody>,
) -> AppResult<Json<AccountSummary>> {
    let acct = account::find_by_id(&state.db, id)
        .await?
        .ok_or(AppError::NotFound)?;

    let new_type = match body.account_type.as_deref() {
        Some(s) => Some(
            AccountType::parse(s)
                .ok_or_else(|| AppError::BadRequest(format!("unknown account_type {s:?}")))?,
        ),
        None => None,
    };

    if matches!(new_type, Some(AccountType::Child))
        && matches!(acct.typed(), AccountType::Parent)
        && account::parent_count(&state.db).await? <= 1
    {
        return Err(AppError::BadRequest(
            "cannot demote the last parent account".into(),
        ));
    }

    account::update(&state.db, id, body.display_name.as_deref(), new_type).await?;
    let updated = account::find_by_id(&state.db, id)
        .await?
        .ok_or(AppError::NotFound)?;
    Ok(Json(AccountSummary::from(&updated)))
}

/// `DELETE /api/accounts/:id` — remove an account.
pub async fn delete(State(state): State<AppState>, Path(id): Path<i64>) -> AppResult<StatusCode> {
    let acct = account::find_by_id(&state.db, id)
        .await?
        .ok_or(AppError::NotFound)?;

    if matches!(acct.typed(), AccountType::Parent) && account::parent_count(&state.db).await? <= 1 {
        return Err(AppError::BadRequest(
            "cannot delete the last parent account".into(),
        ));
    }

    account::delete(&state.db, id).await?;
    Ok(StatusCode::NO_CONTENT)
}
