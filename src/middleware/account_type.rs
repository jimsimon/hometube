//! Account-type gating.
//!
//! Many routes are restricted to one role (parent for admin-only APIs,
//! child for play-tracking APIs). These two middlewares run *after*
//! [`crate::middleware::auth::session_layer`] has populated the
//! `CurrentAccount` extension and short-circuit with 401/403 on
//! mismatches.

use axum::{http::Request, middleware::Next, response::IntoResponse, response::Response};

use crate::error::AppError;
use crate::middleware::auth::CurrentAccount;
use crate::models::account::AccountType;

/// Reject requests unless the current session is for a parent account.
pub async fn require_parent(req: Request<axum::body::Body>, next: Next) -> Response {
    match check(&req, AccountType::Parent) {
        Ok(()) => next.run(req).await,
        Err(e) => e.into_response(),
    }
}

/// Reject requests unless the current session is for a child account.
pub async fn require_child(req: Request<axum::body::Body>, next: Next) -> Response {
    match check(&req, AccountType::Child) {
        Ok(()) => next.run(req).await,
        Err(e) => e.into_response(),
    }
}

fn check(req: &Request<axum::body::Body>, want: AccountType) -> Result<(), AppError> {
    let current = req
        .extensions()
        .get::<CurrentAccount>()
        .ok_or(AppError::Unauthorized)?;
    if current.account_type == want {
        Ok(())
    } else {
        Err(AppError::Forbidden)
    }
}
