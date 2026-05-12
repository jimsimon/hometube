//! Authentication routes.
//!
//! These endpoints implement the Google OAuth2 flow, profile switching,
//! and PIN management.
//!
//! - `GET /api/auth/login` — redirect to Google with PKCE + CSRF state
//!   stored in a short-lived signed cookie (`hometube_oauth`)
//! - `GET /api/auth/callback` — exchange the code, upsert into
//!   `accounts`, create a session, set the session cookie
//! - `POST /api/auth/logout` — drop the session row, clear the cookie,
//!   redirect to `/profiles`
//! - `GET /api/auth/me` — return the JSON view of the current account
//! - `GET /api/auth/profiles` — list every account for the profile picker
//! - `POST /api/auth/switch` — switch sessions (PIN-required for parents)
//! - `PUT /api/auth/pin` — set/update the current account's PIN
//!
//! All session/CSRF cookies are signed with the application's master
//! [`tower_cookies::Key`] from [`crate::state::AppState`].

use argon2::password_hash::rand_core::OsRng;
use argon2::password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString};
use argon2::Argon2;
use axum::{
    extract::{Query, State},
    http::StatusCode,
    response::{IntoResponse, Redirect, Response},
    Json,
};
use chrono::Utc;
use oauth2::{PkceCodeVerifier, TokenResponse};
use serde::{Deserialize, Serialize};
use tower_cookies::{cookie::SameSite, Cookie, Cookies};
use tracing::{info, warn};

use crate::error::{AppError, AppResult};
use crate::middleware::auth::{CurrentAccount, SESSION_COOKIE};
use crate::models::account::{self, AccountType};
use crate::models::session;
use crate::routes::family;
use crate::services::oauth as oauth_svc;
use crate::services::setup;
use crate::state::AppState;

/// Short-lived cookie used to round-trip CSRF state, PKCE verifier and
/// requested role from `/login` to `/callback`.
const OAUTH_COOKIE: &str = "hometube_oauth";
const OAUTH_COOKIE_TTL_SECS: i64 = 600;

/// Payload stored (JSON-encoded) in the signed `hometube_oauth` cookie.
#[derive(Debug, Serialize, Deserialize)]
struct OAuthFlowState {
    csrf: String,
    pkce_verifier: String,
    /// Role the user requested (`"parent"` or `"child"`); always
    /// overridden to `parent` if no accounts exist yet.
    role: String,
}

/// Query string accepted by `/api/auth/login` and `/api/auth/callback`.
#[derive(Debug, Deserialize)]
pub struct LoginQuery {
    /// Optional intended role: `parent` or `child`.
    #[serde(default)]
    pub role: Option<String>,
    /// Optional flow context. Currently understood values:
    ///
    /// - `add_member` — add a new family member from `/parent/family`
    /// - `reauth` — re-authenticate an existing account
    ///
    /// In both cases the actual state lives in the
    /// [`family::PENDING_MEMBER_COOKIE`] cookie; the query parameter is
    /// purely informational so the login URL is self-describing.
    /// We accept it here so it doesn't get rejected as an unknown
    /// query parameter, but the server never reads it.
    #[allow(dead_code)]
    #[serde(default)]
    pub context: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct CallbackQuery {
    /// Authorization code on a successful round-trip. `None` when
    /// Google returns an error (the `error` field is set instead).
    #[serde(default)]
    pub code: Option<String>,
    /// CSRF state token on a successful round-trip. `None` on error.
    #[serde(default)]
    pub state: Option<String>,
    /// Set by Google when the user cancels or otherwise can't sign
    /// in (e.g. `access_denied`). Surfaces a redirect to
    /// `/login?error=<code>` instead of a bare 400.
    #[serde(default)]
    pub error: Option<String>,
}

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

/// `GET /api/auth/login?role=parent|child` — kick off the OAuth flow.
pub async fn login(
    State(state): State<AppState>,
    cookies: Cookies,
    Query(q): Query<LoginQuery>,
) -> AppResult<Response> {
    let client = oauth_svc::build_client(&state.db).await?;
    let (auth_url, csrf, pkce_verifier) = oauth_svc::authorize_url(&client);

    let role = q.role.as_deref().unwrap_or("parent").to_string();

    let payload = OAuthFlowState {
        csrf: csrf.secret().to_string(),
        pkce_verifier: pkce_verifier.secret().to_string(),
        role,
    };
    let json = serde_json::to_string(&payload).map_err(|e| AppError::Other(e.into()))?;

    let mut cookie = Cookie::new(OAUTH_COOKIE, json);
    cookie.set_path("/");
    cookie.set_http_only(true);
    cookie.set_same_site(SameSite::Lax);
    cookie.set_max_age(tower_cookies::cookie::time::Duration::seconds(
        OAUTH_COOKIE_TTL_SECS,
    ));
    cookies.signed(&state.cookie_key).add(cookie);

    Ok(Redirect::to(auth_url.as_str()).into_response())
}

/// `GET /api/auth/callback` — finish the OAuth flow.
///
/// The callback supports three distinct flows, distinguished by the
/// presence (and contents) of the [`family::PENDING_MEMBER_COOKIE`]:
///
/// 1. **Default sign-in** — no pending-member cookie. The user is
///    creating or refreshing the account they're signing in with;
///    on success, redirect to `/` (or `/setup` if setup isn't done).
/// 2. **Add member** — pending cookie with `context=add_member`. The
///    new account is created with the cookie's chosen role + display
///    name override, and the user is sent on to `/parent/family`.
/// 3. **Re-authenticate** — pending cookie with `context=reauth` and
///    an `account_id`. Tokens for that existing row are refreshed; if
///    the Google account presented in the callback doesn't match the
///    target row's `google_id` the request is rejected with 400 so a
///    parent can't accidentally swap one child's identity for another.
pub async fn callback(
    State(state): State<AppState>,
    cookies: Cookies,
    Query(q): Query<CallbackQuery>,
) -> AppResult<Response> {
    let signed = cookies.signed(&state.cookie_key);

    // Google returned an error (user cancelled, scope refused, etc.).
    // Clear cookies and bounce to /login so the user has a clear path
    // forward instead of seeing a bare 400.
    if let Some(error_code) = q.error.as_deref() {
        let mut clear = Cookie::new(OAUTH_COOKIE, "");
        clear.set_path("/");
        signed.remove(clear);
        family::clear_pending_cookie(&cookies, &state);
        warn!(error = %error_code, "OAuth callback returned error");
        // Sanitize: only allow safe alphanumeric + underscore chars into
        // the redirect URL to prevent header injection / open-redirect.
        let sanitized: String = error_code
            .chars()
            .filter(|c| c.is_ascii_alphanumeric() || *c == '_')
            .take(64)
            .collect();
        return Ok(Redirect::to(&format!("/login?error={sanitized}")).into_response());
    }

    let flow_state: OAuthFlowState = signed
        .get(OAUTH_COOKIE)
        .and_then(|c| serde_json::from_str(c.value()).ok())
        .ok_or_else(|| AppError::BadRequest("missing or invalid OAuth flow cookie".into()))?;

    // Best-effort cleanup: clear the round-trip cookie regardless of
    // outcome below.
    let mut clear = Cookie::new(OAUTH_COOKIE, "");
    clear.set_path("/");
    signed.remove(clear);

    let pending = family::read_pending_cookie(&cookies, &state);
    // Always clear the pending-member cookie now — even if we error
    // out, we don't want a stale cookie hanging around.
    family::clear_pending_cookie(&cookies, &state);

    let code = q
        .code
        .ok_or_else(|| AppError::BadRequest("OAuth callback missing `code` parameter".into()))?;
    let state_token = q
        .state
        .ok_or_else(|| AppError::BadRequest("OAuth callback missing `state` parameter".into()))?;

    if flow_state.csrf != state_token {
        return Err(AppError::BadRequest("OAuth state mismatch".into()));
    }

    let client = oauth_svc::build_client(&state.db).await?;
    let pkce = PkceCodeVerifier::new(flow_state.pkce_verifier);
    let token = oauth_svc::exchange_code(&client, code, pkce).await?;

    let access_token = token.access_token().secret().to_string();
    let refresh_token = token
        .refresh_token()
        .map(|t| t.secret().to_string())
        .ok_or_else(|| {
            AppError::Other(anyhow::anyhow!(
                "Google did not return a refresh token; ensure access_type=offline + prompt=consent"
            ))
        })?;
    let now = Utc::now().timestamp();
    let expires_at = now
        + token
            .expires_in()
            .map(|d| d.as_secs() as i64)
            .unwrap_or(3600);

    let info = oauth_svc::userinfo(&access_token).await?;
    let pending_display = pending.as_ref().and_then(|p| p.display_name.clone());
    let display_name = pending_display
        .clone()
        .or(info.name.clone())
        .unwrap_or_else(|| info.email.clone());
    let avatar = info.picture.as_deref();

    // ---- Re-auth flow: update an existing row in place. ----
    if let Some(pending_inner) = pending.as_ref() {
        if pending_inner.context == "reauth" {
            let target_id = pending_inner
                .account_id
                .ok_or_else(|| AppError::BadRequest("reauth flow missing account_id".into()))?;
            let target = account::find_by_id(&state.db, target_id)
                .await?
                .ok_or(AppError::NotFound)?;
            if target.google_id != info.sub {
                warn!(
                    target_id,
                    expected = %target.google_id,
                    got = %info.sub,
                    "reauth Google account mismatch"
                );
                return Err(AppError::BadRequest(
                    "Google account mismatch: signed in with a different Google account".into(),
                ));
            }
            account::update_profile_and_tokens(
                &state.db,
                target.id,
                account::ProfileUpdate {
                    email: &info.email,
                    display_name: &display_name,
                    avatar_url: avatar,
                    access_token: &access_token,
                    refresh_token: &refresh_token,
                    token_expires_at: expires_at,
                },
            )
            .await?;
            info!(account_id = target.id, "reauth completed");

            // Don't replace the active session — the parent triggering
            // reauth keeps theirs. Just send them back to the family page.
            let target_url = pending_inner
                .redirect_to
                .clone()
                .unwrap_or_else(|| "/parent/family".into());
            return Ok(Redirect::to(&target_url).into_response());
        }
    }

    // ---- Add-member / default sign-in flow ----
    // First account is always a parent. Otherwise the role comes from
    // the pending cookie (if present + sane) or the OAuth flow cookie.
    let total = account::total_count(&state.db).await?;
    let role_str: String = pending
        .as_ref()
        .and_then(|p| p.role.clone())
        .unwrap_or_else(|| flow_state.role.clone());
    let resolved_role = if total == 0 {
        AccountType::Parent
    } else {
        match role_str.as_str() {
            "parent" => AccountType::Parent,
            "child" => AccountType::Child,
            other => {
                warn!(role = %other, "unknown role on callback; defaulting to parent");
                AccountType::Parent
            }
        }
    };

    let mut newly_created_parent_id: Option<i64> = None;
    let account_id = match account::find_by_google_id(&state.db, &info.sub).await? {
        Some(existing) => {
            account::update_profile_and_tokens(
                &state.db,
                existing.id,
                account::ProfileUpdate {
                    email: &info.email,
                    display_name: &display_name,
                    avatar_url: avatar,
                    access_token: &access_token,
                    refresh_token: &refresh_token,
                    token_expires_at: expires_at,
                },
            )
            .await?;
            existing.id
        }
        None => {
            let id = account::insert(
                &state.db,
                &info.sub,
                &info.email,
                &display_name,
                avatar,
                resolved_role,
                &access_token,
                &refresh_token,
                expires_at,
            )
            .await?;
            if matches!(resolved_role, AccountType::Parent) {
                newly_created_parent_id = Some(id);
            }
            id
        }
    };

    info!(account_id, email = %info.email, "OAuth callback succeeded");

    let sess = session::create(&state.db, account_id).await?;
    set_session_cookie(&signed, &sess.id);

    // Where do we go from here?
    //
    // - If we were in the `add_member` flow and just minted a new
    //   parent, force them through `/setup/pin` first so they can't
    //   skip the PIN step (per the plan's "PIN-required-for-parents"
    //   enforcement note).
    // - Otherwise, honor the pending cookie's `redirect_to` if any.
    // - Otherwise, fall back to `/setup` (until setup is done) or `/`.
    let is_add_member = pending
        .as_ref()
        .map(|p| p.context == "add_member")
        .unwrap_or(false);
    let target = if is_add_member && newly_created_parent_id.is_some() {
        "/setup/pin?for_new_parent=1".to_string()
    } else if let Some(redirect_to) = pending.as_ref().and_then(|p| p.redirect_to.clone()) {
        redirect_to
    } else if setup::is_setup_complete(&state.db).await? {
        "/".to_string()
    } else {
        "/setup".to_string()
    };
    Ok(Redirect::to(&target).into_response())
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
fn is_valid_pin(pin: &str) -> bool {
    let len = pin.len();
    (4..=6).contains(&len) && pin.chars().all(|c| c.is_ascii_digit())
}

fn hash_pin(pin: &str) -> AppResult<String> {
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
