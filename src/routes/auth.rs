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
}

#[derive(Debug, Deserialize)]
pub struct CallbackQuery {
    pub code: String,
    pub state: String,
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
pub async fn callback(
    State(state): State<AppState>,
    cookies: Cookies,
    Query(q): Query<CallbackQuery>,
) -> AppResult<Response> {
    let signed = cookies.signed(&state.cookie_key);

    let flow_state: OAuthFlowState = signed
        .get(OAUTH_COOKIE)
        .and_then(|c| serde_json::from_str(c.value()).ok())
        .ok_or_else(|| AppError::BadRequest("missing or invalid OAuth flow cookie".into()))?;

    // Best-effort cleanup: clear the round-trip cookie regardless of
    // outcome below.
    let mut clear = Cookie::new(OAUTH_COOKIE, "");
    clear.set_path("/");
    signed.remove(clear);

    if flow_state.csrf != q.state {
        return Err(AppError::BadRequest("OAuth state mismatch".into()));
    }

    let client = oauth_svc::build_client(&state.db).await?;
    let pkce = PkceCodeVerifier::new(flow_state.pkce_verifier);
    let token = oauth_svc::exchange_code(&client, q.code, pkce).await?;

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
    let display_name = info.name.clone().unwrap_or_else(|| info.email.clone());
    let avatar = info.picture.as_deref();

    // First account is always a parent.
    let total = account::total_count(&state.db).await?;
    let resolved_role = if total == 0 {
        AccountType::Parent
    } else {
        match flow_state.role.as_str() {
            "parent" => AccountType::Parent,
            "child" => AccountType::Child,
            other => {
                warn!(role = %other, "unknown role on callback; defaulting to parent");
                AccountType::Parent
            }
        }
    };

    let account_id = match account::find_by_google_id(&state.db, &info.sub).await? {
        Some(existing) => {
            account::update_profile_and_tokens(
                &state.db,
                existing.id,
                &info.email,
                &display_name,
                avatar,
                &access_token,
                &refresh_token,
                expires_at,
            )
            .await?;
            existing.id
        }
        None => {
            account::insert(
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
            .await?
        }
    };

    info!(account_id, email = %info.email, "OAuth callback succeeded");

    let sess = session::create(&state.db, account_id).await?;
    set_session_cookie(&signed, &sess.id);

    let target = if setup::is_setup_complete(&state.db).await? {
        "/"
    } else {
        "/setup"
    };
    Ok(Redirect::to(target).into_response())
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
        let stored = target
            .pin_hash
            .as_deref()
            .ok_or_else(|| AppError::BadRequest("parent has no PIN configured".into()))?;
        verify_pin(stored, provided)?;
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
