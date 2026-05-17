//! Authentication routes.
//!
//! These endpoints implement PIN-based authentication, profile switching,
//! and account registration.
//!
//! - `POST /api/auth/register` — create the first parent account during
//!   setup (name + PIN), or create additional accounts via the family
//!   management flow
//! - `POST /api/auth/logout` — drop the session row, clear the cookie,
//!   redirect to `/profiles`
//! - `GET /api/auth/me` — return the JSON view of the current account
//! - `GET /api/auth/profiles` — list every account for the profile picker
//! - `POST /api/auth/switch` — switch sessions (PIN-required for parents)
//! - `PUT /api/auth/pin` — set/update the current account's PIN
//!
//! All session cookies are signed with the application's master
//! [`tower_cookies::Key`] from [`crate::state::AppState`].

use argon2::password_hash::rand_core::OsRng;
use argon2::password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString};
use argon2::Argon2;
use axum::{
    extract::State,
    http::StatusCode,
    response::{IntoResponse, Redirect, Response},
    Json,
};
use chrono::Utc;
use serde::{Deserialize, Serialize};
use tower_cookies::{cookie::SameSite, Cookie, Cookies};
use tracing::{info, warn};

use crate::error::{AppError, AppResult};
use crate::middleware::auth::{CurrentAccount, SESSION_COOKIE};
use crate::models::account::{self, AccountType};
use crate::models::session;
use crate::state::AppState;

#[derive(Debug, Deserialize)]
pub struct SwitchBody {
    pub account_id: i64,
    #[serde(default)]
    pub pin: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct PinBody {
    pub pin: String,
}

/// Body for `POST /api/auth/register`.
#[derive(Debug, Deserialize)]
pub struct RegisterBody {
    pub display_name: String,
    /// Required for parent accounts, ignored for children.
    #[serde(default)]
    pub pin: Option<String>,
    /// `"parent"` or `"child"`. Defaults to `"parent"`.
    #[serde(default)]
    pub role: Option<String>,
}

/// Response from `POST /api/auth/register`.
#[derive(Debug, Serialize)]
pub struct RegisterResponse {
    pub account: account::AccountSummary,
}

/// `POST /api/auth/register` — create the initial parent account during
/// first-time setup.
///
/// This endpoint is intentionally **unauthenticated** so the setup
/// wizard can create the first parent before any session exists.  To
/// prevent abuse, it is restricted to only work when **no accounts
/// exist** in the database.  After the first parent is created, all
/// subsequent account creation must go through the parent-guarded
/// `POST /api/family/members` endpoint.
///
/// - The role is always forced to `parent` (since this is the first account).
/// - A 4–6 digit PIN is required.
/// - A session cookie is set for the newly created account.
pub async fn register(
    State(state): State<AppState>,
    cookies: Cookies,
    Json(body): Json<RegisterBody>,
) -> AppResult<Response> {
    let display_name = body.display_name.trim().to_string();
    if display_name.is_empty() {
        return Err(AppError::BadRequest("display_name is required".into()));
    }

    let pin = body.pin.as_deref().unwrap_or("");
    if !is_valid_pin(pin) {
        return Err(AppError::BadRequest(
            "PIN must be 4-6 numeric digits".into(),
        ));
    }

    // Atomically insert the first parent account. The INSERT succeeds
    // only if the accounts table is empty, eliminating the TOCTOU race
    // between a count check and the insert.
    let id = account::insert_first_account(&state.db, &display_name, AccountType::Parent)
        .await?
        .ok_or_else(|| {
            AppError::BadRequest(
                "registration is only available during initial setup; \
                 use the family management screen to add accounts"
                    .into(),
            )
        })?;
    info!(account_id = id, %display_name, "registered first parent account");

    // Hash and persist the PIN.
    let hashed = hash_pin(pin)?;
    account::set_pin_hash(&state.db, id, &hashed).await?;

    let sess = session::create(&state.db, id).await?;
    let signed = cookies.signed(&state.cookie_key);
    set_session_cookie(&signed, &sess.id);

    let acct = account::find_by_id(&state.db, id)
        .await?
        .ok_or(AppError::NotFound)?;
    Ok(Json(RegisterResponse {
        account: account::AccountSummary::from(&acct),
    })
    .into_response())
}

/// `POST /api/auth/logout` — drop the session row + cookie.
pub async fn logout(
    State(state): State<AppState>,
    cookies: Cookies,
    current: Option<CurrentAccount>,
) -> AppResult<Response> {
    let signed = cookies.signed(&state.cookie_key);
    if let Some(c) = current {
        let _ = session::delete(&state.db, &c.session_id).await;
    }
    let mut clear = Cookie::new(SESSION_COOKIE, "");
    clear.set_path("/");
    signed.remove(clear);
    Ok(Redirect::to("/profiles").into_response())
}

/// `GET /api/auth/me` — return the JSON profile for the active session.
pub async fn me(
    State(state): State<AppState>,
    current: CurrentAccount,
) -> AppResult<Json<account::AccountSummary>> {
    let acct = account::find_by_id(&state.db, current.id)
        .await?
        .ok_or(AppError::Unauthorized)?;
    Ok(Json(account::AccountSummary::from(&acct)))
}

/// `GET /api/auth/profiles` — list every account (for the profile picker).
pub async fn profiles(
    State(state): State<AppState>,
) -> AppResult<Json<Vec<account::ProfileSummary>>> {
    Ok(Json(account::list_profiles(&state.db).await?))
}

/// `POST /api/auth/switch` — switch the current session to another
/// account. Parents must supply a matching PIN.
///
/// Failed PIN attempts emit a `tracing::warn!` and, when the rate
/// crosses 5 failures within a 5-minute window, also create a
/// `system_update` notification for every parent so the family is made
/// aware. The rate-limit check is best-effort and never blocks the
/// switch decision itself.
pub async fn switch(
    State(state): State<AppState>,
    cookies: Cookies,
    current: Option<CurrentAccount>,
    Json(body): Json<SwitchBody>,
) -> AppResult<Response> {
    let target = account::find_by_id(&state.db, body.account_id)
        .await?
        .ok_or(AppError::NotFound)?;

    if matches!(target.typed(), AccountType::Parent) {
        let provided = body.pin.as_deref().unwrap_or("");
        let stored = target.pin_hash.as_deref().ok_or_else(|| {
            AppError::BadRequest(
                "this parent profile doesn't have a PIN yet; visit /setup/pin while signed in"
                    .into(),
            )
        })?;
        if let Err(err) = verify_pin(stored, provided) {
            warn!(account_id = target.id, "PIN attempt failed");
            // Track recent failures via parent_notifications metadata.
            // We never fail the request because of this bookkeeping.
            if let Err(e) = record_failed_pin(&state, target.id).await {
                warn!(error = %e, "could not record failed PIN attempt");
            }
            return Err(err);
        }
    }

    // Drop the old session row, if any.
    let signed = cookies.signed(&state.cookie_key);
    if let Some(c) = current {
        let _ = session::delete(&state.db, &c.session_id).await;
    }

    let sess = session::create(&state.db, target.id).await?;
    set_session_cookie(&signed, &sess.id);

    Ok((StatusCode::OK, Json(account::AccountSummary::from(&target))).into_response())
}

/// Insert a soft "system_update" notification for every parent if more
/// than five PIN failures have happened against `target_id` in the
/// past five minutes.
///
/// Throttling: we only count rows where the JSON metadata contains both
/// `"kind":"pin_attempt_failed"` and `"target_account_id":<target_id>`.
/// Every 5th failure within the window emits a fresh notification.
async fn record_failed_pin(state: &AppState, target_id: i64) -> AppResult<()> {
    let now = Utc::now().timestamp();
    let window_start = now - 5 * 60;
    let metadata = serde_json::json!({
        "kind": "pin_attempt_failed",
        "target_account_id": target_id,
        "at": now,
    });

    // Count recent failures from `parent_notifications` metadata. The
    // metadata column is JSON — we use a substring filter so we don't
    // need an extension. False positives are fine, this is only a soft
    // alert.
    let recent_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM parent_notifications \
         WHERE notification_type = ? \
           AND metadata LIKE '%pin_attempt_failed%' \
           AND metadata LIKE ? \
           AND created_at >= ?",
    )
    .bind(crate::services::notifications::TYPE_SYSTEM_UPDATE)
    .bind(format!("%\"target_account_id\":{target_id}%"))
    .bind(window_start)
    .fetch_one(&state.db)
    .await?;

    // Only spam parents on every 5th failure within the window.
    if recent_count >= 5 && recent_count % 5 != 0 {
        return Ok(());
    }

    let message = format!(
        "Someone tried to switch to a parent profile (id {target_id}) and entered the wrong PIN."
    );
    crate::services::notifications::broadcast(
        &state.db,
        crate::services::notifications::TYPE_SYSTEM_UPDATE,
        "Failed PIN attempt",
        &message,
        &metadata,
    )
    .await
}

/// `PUT /api/auth/pin` — set/update the current account's PIN.
pub async fn set_pin(
    State(state): State<AppState>,
    current: CurrentAccount,
    Json(body): Json<PinBody>,
) -> AppResult<StatusCode> {
    if !is_valid_pin(&body.pin) {
        return Err(AppError::BadRequest(
            "PIN must be 4-6 numeric digits".into(),
        ));
    }
    let hashed = hash_pin(&body.pin)?;
    account::set_pin_hash(&state.db, current.id, &hashed).await?;
    Ok(StatusCode::NO_CONTENT)
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Persist the session ID in a signed, secure cookie.
fn set_session_cookie(signed: &tower_cookies::SignedCookies<'_>, session_id: &str) {
    let mut cookie = Cookie::new(SESSION_COOKIE, session_id.to_string());
    cookie.set_path("/");
    cookie.set_http_only(true);
    cookie.set_same_site(SameSite::Lax);
    // Match `session::DEFAULT_SESSION_DAYS` (7 days).
    cookie.set_max_age(tower_cookies::cookie::time::Duration::days(
        crate::models::session::DEFAULT_SESSION_DAYS,
    ));
    signed.add(cookie);
}

/// True iff `pin` is between 4 and 6 ASCII digits.
pub fn is_valid_pin(pin: &str) -> bool {
    let len = pin.len();
    (4..=6).contains(&len) && pin.chars().all(|c| c.is_ascii_digit())
}

pub fn hash_pin(pin: &str) -> AppResult<String> {
    let salt = SaltString::generate(&mut OsRng);
    let argon2 = Argon2::default();
    let hashed = argon2
        .hash_password(pin.as_bytes(), &salt)
        .map_err(|e| AppError::Other(anyhow::anyhow!("hashing PIN: {e}")))?;
    Ok(hashed.to_string())
}

fn verify_pin(hash: &str, pin: &str) -> AppResult<()> {
    let parsed = PasswordHash::new(hash)
        .map_err(|e| AppError::Other(anyhow::anyhow!("invalid PIN hash: {e}")))?;
    Argon2::default()
        .verify_password(pin.as_bytes(), &parsed)
        .map_err(|_| AppError::Forbidden)
}
