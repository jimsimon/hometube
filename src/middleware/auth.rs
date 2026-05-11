//! Session-validation middleware.
//!
//! On every request, this middleware reads the signed `hometube_session`
//! cookie, looks up the matching session row, and stuffs the resulting
//! [`CurrentAccount`] into request extensions. Handlers then either pull
//! the extractor (mandatory) or read the extension manually for optional
//! authentication.

use axum::extract::{FromRequestParts, OptionalFromRequestParts};
use axum::http::request::Parts;
use axum::{extract::State, http::Request, middleware::Next, response::Response};
use tower_cookies::{Cookie, Cookies};
use tracing::debug;

use crate::error::AppError;
use crate::models::account::{Account, AccountType};
use crate::models::session;
use crate::state::AppState;

/// Name of the signed cookie that carries the random session ID.
pub const SESSION_COOKIE: &str = "hometube_session";

/// Snapshot of the currently-authenticated account, populated by
/// [`session_layer`] and consumed via the [`CurrentAccount`] extractor.
#[derive(Debug, Clone)]
pub struct CurrentAccount {
    pub id: i64,
    pub display_name: String,
    pub email: String,
    pub avatar_url: Option<String>,
    pub account_type: AccountType,
    pub session_id: String,
}

impl From<(&Account, String)> for CurrentAccount {
    fn from((a, session_id): (&Account, String)) -> Self {
        Self {
            id: a.id,
            display_name: a.display_name.clone(),
            email: a.email.clone(),
            avatar_url: a.avatar_url.clone(),
            account_type: a.typed(),
            session_id,
        }
    }
}

/// Middleware that resolves the session cookie into a [`CurrentAccount`]
/// extension. Always invokes the inner service — pages and APIs that
/// require authentication must enforce it themselves via the extractor.
pub async fn session_layer(
    State(state): State<AppState>,
    cookies: Cookies,
    mut req: Request<axum::body::Body>,
    next: Next,
) -> Response {
    let signed = cookies.signed(&state.cookie_key);
    if let Some(cookie) = signed.get(SESSION_COOKIE) {
        let session_id = cookie.value().to_string();
        match session::lookup_with_account(&state.db, &session_id).await {
            Ok(Some((sess, account))) => {
                let current = CurrentAccount::from((&account, sess.id.clone()));
                req.extensions_mut().insert(current);
            }
            Ok(None) => {
                // Stale or unknown session — clear the cookie so we don't
                // keep paying the DB roundtrip.
                debug!(%session_id, "stale session cookie; clearing");
                let mut clear = Cookie::new(SESSION_COOKIE, "");
                clear.set_path("/");
                signed.remove(clear);
            }
            Err(err) => {
                debug!(error = %err, "session lookup failed");
            }
        }
    }
    next.run(req).await
}

/// Mandatory extractor: returns 401 if no session is attached to the
/// request. The companion [`OptionalFromRequestParts`] impl returns
/// `None` instead, for routes that allow anonymous access.
impl<S> FromRequestParts<S> for CurrentAccount
where
    S: Send + Sync,
{
    type Rejection = AppError;

    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self, Self::Rejection> {
        parts
            .extensions
            .get::<CurrentAccount>()
            .cloned()
            .ok_or(AppError::Unauthorized)
    }
}

impl<S> OptionalFromRequestParts<S> for CurrentAccount
where
    S: Send + Sync,
{
    type Rejection = std::convert::Infallible;

    async fn from_request_parts(
        parts: &mut Parts,
        _state: &S,
    ) -> Result<Option<Self>, Self::Rejection> {
        Ok(parts.extensions.get::<CurrentAccount>().cloned())
    }
}
