//! Family-management routes (parent only).
//!
//! Endpoints:
//!
//! - `GET    /api/family/members` — list every account with the timestamp
//!   of its most recent session
//! - `POST   /api/family/members` — create a new family member (parent or
//!   child). Parents are created locally with a name + PIN; children with
//!   a name only.
//! - `PUT    /api/family/members/:id` — rename and/or change role,
//!   refusing to demote the only parent
//! - `DELETE /api/family/members/:id` — remove an account, refusing to
//!   delete the only parent

use axum::{
    extract::{Path, State},
    http::StatusCode,
    Json,
};
use serde::{Deserialize, Serialize};
use tracing::info;

use crate::error::{AppError, AppResult};
use crate::models::account::{self, AccountType};
use crate::routes::auth;
use crate::state::AppState;

/// Single row in `GET /api/family/members`.
#[derive(Debug, Serialize)]
pub struct FamilyMember {
    pub id: i64,
    pub display_name: String,
    pub avatar_url: Option<String>,
    pub account_type: String,
    pub has_pin: bool,
    pub created_at: i64,
    /// `created_at` of the most recent `sessions` row, or `None` if the
    /// account has never signed in.
    pub last_login_at: Option<i64>,
}

#[derive(Debug, Deserialize)]
pub struct AddMemberBody {
    /// `"parent"` or `"child"`.
    pub role: String,
    #[serde(default)]
    pub display_name: Option<String>,
    /// Required for parent accounts.
    #[serde(default)]
    pub pin: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct UpdateMemberBody {
    #[serde(default)]
    pub display_name: Option<String>,
    /// `"parent"` or `"child"`.
    #[serde(default)]
    pub role: Option<String>,
}

/// `GET /api/family/members` — return every account with derived
/// last-login fields.
pub async fn list_members(State(state): State<AppState>) -> AppResult<Json<Vec<FamilyMember>>> {
    let accounts = account::list_all(&state.db).await?;

    let mut members = Vec::with_capacity(accounts.len());
    for a in accounts.iter() {
        let last_login_at: Option<i64> =
            sqlx::query_scalar("SELECT MAX(created_at) FROM sessions WHERE account_id = ?")
                .bind(a.id)
                .fetch_one(&state.db)
                .await?;

        members.push(FamilyMember {
            id: a.id,
            display_name: a.display_name.clone(),
            avatar_url: a.avatar_url.clone(),
            account_type: a.account_type.clone(),
            has_pin: a.pin_hash.is_some(),
            created_at: a.created_at,
            last_login_at,
        });
    }

    Ok(Json(members))
}

/// `POST /api/family/members` — create a new family member locally.
/// Parents require a display name and PIN; children require only a name.
pub async fn add_member(
    State(state): State<AppState>,
    Json(body): Json<AddMemberBody>,
) -> AppResult<Json<FamilyMember>> {
    let role = body.role.trim().to_ascii_lowercase();
    let parsed_role = AccountType::parse(&role).ok_or_else(|| {
        AppError::BadRequest(format!(
            "unknown role {:?}; expected \"parent\" or \"child\"",
            body.role
        ))
    })?;

    let display_name = body
        .display_name
        .as_deref()
        .filter(|s| !s.trim().is_empty())
        .ok_or_else(|| AppError::BadRequest("display_name is required".into()))?;

    // Parents must supply a valid PIN.
    if parsed_role == AccountType::Parent {
        let pin = body.pin.as_deref().unwrap_or("");
        if !auth::is_valid_pin(pin) {
            return Err(AppError::BadRequest(
                "PIN must be 4-6 numeric digits".into(),
            ));
        }
    }

    let id = account::insert_local(&state.db, display_name, None, parsed_role).await?;

    // Hash and persist the PIN for parent accounts.
    if parsed_role == AccountType::Parent {
        if let Some(ref pin) = body.pin {
            let hashed = auth::hash_pin(pin)?;
            account::set_pin_hash(&state.db, id, &hashed).await?;
        }
    }

    info!(account_id = id, %display_name, role = %role, "created family member");
    list_one(&state, id).await.map(Json)
}

/// `PUT /api/family/members/:id` — rename / change role.
pub async fn update_member(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    Json(body): Json<UpdateMemberBody>,
) -> AppResult<Json<FamilyMember>> {
    let acct = account::find_by_id(&state.db, id)
        .await?
        .ok_or(AppError::NotFound)?;

    let new_type = match body.role.as_deref() {
        Some(s) => Some(
            AccountType::parse(s)
                .ok_or_else(|| AppError::BadRequest(format!("unknown role {s:?}")))?,
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

    // Re-fetch + enrich for the response.
    list_one(&state, id).await.map(Json)
}

/// `DELETE /api/family/members/:id` — refuse to delete the last parent;
/// otherwise drop the account row (sessions will be cleaned up by FK).
pub async fn delete_member(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> AppResult<StatusCode> {
    let acct = account::find_by_id(&state.db, id)
        .await?
        .ok_or(AppError::NotFound)?;

    if matches!(acct.typed(), AccountType::Parent) && account::parent_count(&state.db).await? <= 1 {
        return Err(AppError::BadRequest(
            "cannot delete the last parent account".into(),
        ));
    }

    // Best-effort: drop sessions first so the FK on `sessions.account_id`
    // doesn't reject the delete on databases that have FK enforcement
    // turned on.
    let _ = sqlx::query("DELETE FROM sessions WHERE account_id = ?")
        .bind(id)
        .execute(&state.db)
        .await;

    account::delete(&state.db, id).await?;
    info!(account_id = id, "deleted family member");
    Ok(StatusCode::NO_CONTENT)
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

async fn list_one(state: &AppState, id: i64) -> AppResult<FamilyMember> {
    let a = account::find_by_id(&state.db, id)
        .await?
        .ok_or(AppError::NotFound)?;
    let last_login_at: Option<i64> =
        sqlx::query_scalar("SELECT MAX(created_at) FROM sessions WHERE account_id = ?")
            .bind(a.id)
            .fetch_one(&state.db)
            .await?;
    Ok(FamilyMember {
        id: a.id,
        display_name: a.display_name.clone(),
        avatar_url: a.avatar_url.clone(),
        account_type: a.account_type.clone(),
        has_pin: a.pin_hash.is_some(),
        created_at: a.created_at,
        last_login_at,
    })
}
