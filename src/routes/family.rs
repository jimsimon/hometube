//! Family-management routes (parent only).
//!
//! Phase 13 builds a dedicated parent-facing view of every account in
//! the system, with the controls required to add new family members,
//! re-authenticate stale ones, and remove (or rename) anyone but the
//! last remaining parent.
//!
//! Endpoints:
//!
//! - `GET    /api/family/members` — list every account with a
//!   `token_expired` flag and the timestamp of its most recent session
//! - `POST   /api/family/members` — kick off the OAuth flow that adds a
//!   *new* family member; the desired role is round-tripped through a
//!   short-lived signed cookie ([`PENDING_MEMBER_COOKIE`])
//! - `PUT    /api/family/members/:id` — rename and/or change role,
//!   refusing to demote the only parent
//! - `DELETE /api/family/members/:id` — remove an account, refusing to
//!   delete the only parent
//! - `POST   /api/family/members/:id/reauth` — restart the OAuth flow
//!   for an existing account so its tokens get refreshed

use axum::{
    extract::{Path, State},
    http::StatusCode,
    Json,
};
use chrono::Utc;
use serde::{Deserialize, Serialize};
use tower_cookies::{cookie::SameSite, Cookie, Cookies};
use tracing::info;

use crate::error::{AppError, AppResult};
use crate::models::account::{self, AccountType};
use crate::state::AppState;

/// Short-lived signed cookie used to round-trip "I am about to add a
/// new family member with role X" through the Google OAuth callback.
pub const PENDING_MEMBER_COOKIE: &str = "hometube_pending_member";
/// Same TTL as the regular OAuth flow cookie — 10 minutes is plenty.
pub const PENDING_MEMBER_TTL_SECS: i64 = 600;

/// Value persisted (JSON-encoded) in [`PENDING_MEMBER_COOKIE`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PendingMember {
    /// `add_member` for the "create new account" flow,
    /// `reauth` for the "refresh existing account's tokens" flow.
    pub context: String,
    /// Desired role for the new account. Ignored for `reauth`.
    #[serde(default)]
    pub role: Option<String>,
    /// Optional override for the account's display name.
    #[serde(default)]
    pub display_name: Option<String>,
    /// Existing account ID for `reauth`.
    #[serde(default)]
    pub account_id: Option<i64>,
    /// Where to send the user after the callback finishes. Defaults to
    /// `/parent/family` for the family flow.
    #[serde(default)]
    pub redirect_to: Option<String>,
}

/// Single row in `GET /api/family/members`.
#[derive(Debug, Serialize)]
pub struct FamilyMember {
    pub id: i64,
    pub email: String,
    pub display_name: String,
    pub avatar_url: Option<String>,
    pub account_type: String,
    pub has_pin: bool,
    pub created_at: i64,
    pub token_expires_at: i64,
    pub token_expired: bool,
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
}

#[derive(Debug, Serialize)]
pub struct LoginUrlResponse {
    pub login_url: String,
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
/// token-expiry / last-login fields.
pub async fn list_members(State(state): State<AppState>) -> AppResult<Json<Vec<FamilyMember>>> {
    // Pull every account, then enrich with a single "max(sessions.created_at)"
    // query. Two separate queries keep the SQL simple and survive the
    // case where an account has never had a session row.
    let accounts = account::list_all(&state.db).await?;

    let now = Utc::now().timestamp();
    let mut members = Vec::with_capacity(accounts.len());
    for a in accounts.iter() {
        // MAX() always returns a single row, but the value is NULL when
        // the account has never had a session. The outer `Option`
        // covers the column nullability.
        let last_login_at: Option<i64> =
            sqlx::query_scalar("SELECT MAX(created_at) FROM sessions WHERE account_id = ?")
                .bind(a.id)
                .fetch_one(&state.db)
                .await?;

        members.push(FamilyMember {
            id: a.id,
            email: a.email.clone(),
            display_name: a.display_name.clone(),
            avatar_url: a.avatar_url.clone(),
            account_type: a.account_type.clone(),
            has_pin: a.pin_hash.is_some(),
            created_at: a.created_at,
            token_expires_at: a.token_expires_at,
            token_expired: a.token_expires_at < now,
            last_login_at,
        });
    }

    Ok(Json(members))
}

/// Response for add-member: either a login_url (for parents who must
/// OAuth) or the created member (for local-only children).
#[derive(Debug, Serialize)]
#[serde(untagged)]
pub enum AddMemberResponse {
    /// Parent account — browser must navigate to this URL for OAuth.
    Redirect(LoginUrlResponse),
    /// Child account — created immediately as local-only.
    Created(FamilyMember),
}

/// `POST /api/family/members` — for children, creates a local-only
/// account immediately and returns the new member. For parents, stores
/// a pending-member cookie and returns a login URL for the OAuth flow.
pub async fn add_member(
    State(state): State<AppState>,
    cookies: Cookies,
    Json(body): Json<AddMemberBody>,
) -> AppResult<Json<AddMemberResponse>> {
    let role = body.role.trim().to_ascii_lowercase();
    let parsed_role = AccountType::parse(&role).ok_or_else(|| {
        AppError::BadRequest(format!(
            "unknown role {:?}; expected \"parent\" or \"child\"",
            body.role
        ))
    })?;

    // Children are created as local-only accounts — no Google sign-in needed.
    if parsed_role == AccountType::Child {
        let display_name = body
            .display_name
            .as_deref()
            .filter(|s| !s.trim().is_empty())
            .ok_or_else(|| {
                AppError::BadRequest("display_name is required for child accounts".into())
            })?;
        let id = account::insert_local_child(&state.db, display_name, None).await?;
        info!(account_id = id, display_name, "created local child account");
        let member = list_one(&state, id).await?;
        return Ok(Json(AddMemberResponse::Created(member)));
    }

    // Parents still require Google OAuth for identity.
    let pending = PendingMember {
        context: "add_member".into(),
        role: Some(role.clone()),
        display_name: body.display_name.clone(),
        account_id: None,
        redirect_to: Some("/parent/family".into()),
    };
    set_pending_cookie(&cookies, &state, &pending)?;

    let login_url = format!("/api/auth/login?role={role}&context=add_member");
    info!(role = %role, "queued add_member flow");
    Ok(Json(AddMemberResponse::Redirect(LoginUrlResponse {
        login_url,
    })))
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

/// `POST /api/family/members/:id/reauth` — start the OAuth flow but
/// keep the account ID in the round-trip cookie so the callback updates
/// the existing row's tokens instead of creating a fresh account.
pub async fn reauth_member(
    State(state): State<AppState>,
    cookies: Cookies,
    Path(id): Path<i64>,
) -> AppResult<Json<LoginUrlResponse>> {
    let acct = account::find_by_id(&state.db, id)
        .await?
        .ok_or(AppError::NotFound)?;

    let pending = PendingMember {
        context: "reauth".into(),
        role: Some(acct.account_type.clone()),
        display_name: None,
        account_id: Some(acct.id),
        redirect_to: Some("/parent/family".into()),
    };
    set_pending_cookie(&cookies, &state, &pending)?;

    let role = acct.account_type;
    let login_url = format!("/api/auth/login?role={role}&context=reauth");
    info!(account_id = id, "queued reauth flow");
    Ok(Json(LoginUrlResponse { login_url }))
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Read the pending-member cookie back out (if present) — invoked by
/// `auth::callback`.
pub fn read_pending_cookie(cookies: &Cookies, state: &AppState) -> Option<PendingMember> {
    let signed = cookies.signed(&state.cookie_key);
    let cookie = signed.get(PENDING_MEMBER_COOKIE)?;
    let parsed = serde_json::from_str(cookie.value()).ok()?;
    Some(parsed)
}

/// Drop the pending-member cookie (called by `auth::callback` after the
/// flow completes, regardless of success).
pub fn clear_pending_cookie(cookies: &Cookies, state: &AppState) {
    let signed = cookies.signed(&state.cookie_key);
    let mut clear = Cookie::new(PENDING_MEMBER_COOKIE, "");
    clear.set_path("/");
    signed.remove(clear);
}

fn set_pending_cookie(
    cookies: &Cookies,
    state: &AppState,
    pending: &PendingMember,
) -> AppResult<()> {
    let json = serde_json::to_string(pending)
        .map_err(|e| AppError::Other(anyhow::anyhow!("serialising pending member: {e}")))?;
    let mut cookie = Cookie::new(PENDING_MEMBER_COOKIE, json);
    cookie.set_path("/");
    cookie.set_http_only(true);
    cookie.set_same_site(SameSite::Lax);
    cookie.set_max_age(tower_cookies::cookie::time::Duration::seconds(
        PENDING_MEMBER_TTL_SECS,
    ));
    cookies.signed(&state.cookie_key).add(cookie);
    Ok(())
}

async fn list_one(state: &AppState, id: i64) -> AppResult<FamilyMember> {
    let a = account::find_by_id(&state.db, id)
        .await?
        .ok_or(AppError::NotFound)?;
    let last_login_at: Option<i64> =
        sqlx::query_scalar("SELECT MAX(created_at) FROM sessions WHERE account_id = ?")
            .bind(a.id)
            .fetch_one(&state.db)
            .await?;
    let now = Utc::now().timestamp();
    Ok(FamilyMember {
        id: a.id,
        email: a.email.clone(),
        display_name: a.display_name.clone(),
        avatar_url: a.avatar_url.clone(),
        account_type: a.account_type.clone(),
        has_pin: a.pin_hash.is_some(),
        created_at: a.created_at,
        token_expires_at: a.token_expires_at,
        token_expired: a.token_expires_at < now,
        last_login_at,
    })
}
