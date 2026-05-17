//! Test-only login + fixture endpoints.
//!
//! Compiled in only when the `test-login` Cargo feature is enabled.
//! Production builds MUST NOT include this module — `routes/mod.rs`
//! only mounts it under `#[cfg(feature = "test-login")]` and the CI
//! E2E job is the only place that builds with the feature on.
//!
//! Endpoints:
//!
//!   POST /api/test/seed
//!     Body: { display_name?, role: "parent" | "child" }
//!     Marks setup_complete=true if it isn't already, mints a fresh
//!     account with the requested role, sets a session cookie for it,
//!     and returns the new account's ID.
//!
//!   POST /api/test/login-as
//!     Body: { account_id }
//!     Switches the session cookie to the given account without
//!     verifying any PIN. Used by tests that want to flip between
//!     parent and child seeded by an earlier seed call.
//!
//!   POST /api/test/reset
//!     Wipes all rows from accounts/sessions/app_config so the next
//!     test starts from scratch.

use axum::{extract::State, http::StatusCode, Json};
use serde::{Deserialize, Serialize};
use tower_cookies::{cookie::SameSite, Cookie, Cookies};

use crate::error::{AppError, AppResult};
use crate::middleware::auth::SESSION_COOKIE;
use crate::models::account::AccountType;
use crate::services::setup::{
    set_config_value, KEY_GOOGLE_CLIENT_ID, KEY_GOOGLE_CLIENT_SECRET, KEY_GOOGLE_REDIRECT_URI,
    KEY_SETUP_COMPLETE,
};
use crate::state::AppState;

#[derive(Debug, Deserialize)]
pub struct SeedBody {
    /// `"parent"` or `"child"`. Anything else returns 400.
    pub role: String,
    /// Optional override; defaults to `"E2E Parent"` / `"E2E Child"`.
    #[serde(default)]
    pub display_name: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct SeedResponse {
    pub account_id: i64,
    pub session_id: String,
}

/// `POST /api/test/seed` — create an account + session in one shot.
pub async fn seed(
    State(state): State<AppState>,
    cookies: Cookies,
    Json(body): Json<SeedBody>,
) -> AppResult<Json<SeedResponse>> {
    let role = match body.role.as_str() {
        "parent" => AccountType::Parent,
        "child" => AccountType::Child,
        other => {
            return Err(AppError::BadRequest(format!(
                "unknown role `{other}`; expected `parent` or `child`"
            )))
        }
    };

    // Always mark setup complete + ensure dummy credentials exist so
    // middleware doesn't bounce us back to /setup. Idempotent.
    let _ = set_config_value(&state.db, KEY_GOOGLE_CLIENT_ID, "test-client-id").await;
    let _ = set_config_value(&state.db, KEY_GOOGLE_CLIENT_SECRET, "test-client-secret").await;
    let _ = set_config_value(
        &state.db,
        KEY_GOOGLE_REDIRECT_URI,
        "http://localhost:3000/api/auth/callback",
    )
    .await;
    let _ = set_config_value(&state.db, KEY_SETUP_COMPLETE, "true").await;

    let display_name = body.display_name.unwrap_or_else(|| match role {
        AccountType::Parent => "E2E Parent".into(),
        AccountType::Child => "E2E Child".into(),
    });

    // Random suffix so multiple seed() calls in the same DB don't
    // collide on the UNIQUE(google_id) constraint.
    let nonce: u64 = rand::random();
    let google_id = format!("e2e-{nonce}");
    let email = format!("{nonce}@e2e.test");

    let now = chrono::Utc::now().timestamp();
    let account_id: i64 = sqlx::query_scalar(
        "INSERT INTO accounts \
            (google_id, email, display_name, avatar_url, account_type, \
             access_token, refresh_token, token_expires_at) \
         VALUES (?, ?, ?, NULL, ?, 'tk', 'rk', ?) \
         RETURNING id",
    )
    .bind(&google_id)
    .bind(&email)
    .bind(&display_name)
    .bind(role.as_str())
    .bind(now + 3600)
    .fetch_one(&state.db)
    .await?;

    let session_id = mint_session(&state, account_id).await?;
    set_session_cookie(&cookies, &state, &session_id);

    Ok(Json(SeedResponse {
        account_id,
        session_id,
    }))
}

#[derive(Debug, Deserialize)]
pub struct LoginAsBody {
    pub account_id: i64,
}

/// `POST /api/test/login-as` — flip the session to a specific account.
pub async fn login_as(
    State(state): State<AppState>,
    cookies: Cookies,
    Json(body): Json<LoginAsBody>,
) -> AppResult<Json<serde_json::Value>> {
    let exists: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM accounts WHERE id = ?")
        .bind(body.account_id)
        .fetch_one(&state.db)
        .await?;
    if exists == 0 {
        return Err(AppError::NotFound);
    }
    let session_id = mint_session(&state, body.account_id).await?;
    set_session_cookie(&cookies, &state, &session_id);
    Ok(Json(serde_json::json!({ "session_id": session_id })))
}

/// `POST /api/test/reset` — wipe everything except sqlite_* and
/// migration-tracking tables. Dynamically enumerates user tables from
/// `sqlite_master` so this endpoint doesn't drift out of sync when
/// new migrations add tables.
pub async fn reset(State(state): State<AppState>) -> AppResult<StatusCode> {
    // Enumerate all user tables (excludes sqlite internals and the
    // sqlx migrations table).
    let tables: Vec<(String,)> = sqlx::query_as(
        "SELECT name FROM sqlite_master \
         WHERE type = 'table' \
           AND name NOT LIKE 'sqlite_%' \
           AND name != '_sqlx_migrations' \
         ORDER BY name",
    )
    .fetch_all(&state.db)
    .await?;

    // Temporarily disable FK checks so we can DELETE in any order
    // without cascading failures. Re-enable after the loop.
    let _ = sqlx::query("PRAGMA foreign_keys = OFF")
        .execute(&state.db)
        .await;
    for (table,) in &tables {
        let _ = sqlx::query(&format!("DELETE FROM [{table}]"))
            .execute(&state.db)
            .await;
    }
    let _ = sqlx::query("PRAGMA foreign_keys = ON")
        .execute(&state.db)
        .await;
    Ok(StatusCode::NO_CONTENT)
}

async fn mint_session(state: &AppState, account_id: i64) -> AppResult<String> {
    use rand::distr::Alphanumeric;
    use rand::RngExt;
    let session_id: String = rand::rng()
        .sample_iter(Alphanumeric)
        .take(32)
        .map(char::from)
        .collect();
    let expires_at = chrono::Utc::now().timestamp() + 7 * 24 * 3600;
    sqlx::query("INSERT INTO sessions (id, account_id, expires_at) VALUES (?, ?, ?)")
        .bind(&session_id)
        .bind(account_id)
        .bind(expires_at)
        .execute(&state.db)
        .await?;
    Ok(session_id)
}

fn set_session_cookie(cookies: &Cookies, state: &AppState, session_id: &str) {
    let mut cookie = Cookie::new(SESSION_COOKIE, session_id.to_string());
    cookie.set_path("/");
    cookie.set_http_only(true);
    cookie.set_same_site(SameSite::Lax);
    cookies.signed(&state.cookie_key).add(cookie);
}
