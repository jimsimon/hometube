//! Setup-redirect middleware.
//!
//! Until `setup_complete` flips to `true` in `app_config`, every page
//! request that isn't part of the setup flow itself is redirected to
//! `/setup`. The allow-list mirrors the routes the wizard needs to
//! function: the wizard page, its API endpoints, the OAuth callback
//! (so the "Sign in with Google" step can finish), and static assets.

use axum::{
    extract::State,
    http::{Request, StatusCode, Uri},
    middleware::Next,
    response::{IntoResponse, Redirect, Response},
};

use crate::services::setup;
use crate::state::AppState;

/// Path prefixes that are always reachable, even before setup is
/// complete. The `/api/auth` prefix is allowed so the OAuth flow can
/// finish (the `Sign in with Google` button in the wizard kicks off
/// `/api/auth/login` and Google redirects back to `/api/auth/callback`),
/// and so the wizard can ask `/api/auth/me` whether the parent has
/// already signed in.
const ALLOWED_PREFIXES: &[&str] = &[
    "/setup",
    "/api/setup",
    "/api/auth",
    "/api/health",
    "/assets",
    // Service worker + offline fallback page must always be reachable
    // so PWA installs and offline detection still work during setup.
    "/sw.js",
    "/offline.html",
    "/manifest.webmanifest",
    // Phase 15: profile picker + per-parent PIN-set page must be
    // reachable independently of the wizard, but they only matter
    // *after* setup completes (the wizard handles its own multi-account
    // flow). Allowing them here is harmless during setup since the
    // pages themselves redirect to /setup when no parent exists.
    "/profiles",
    "/login",
    // E2E test-login routes must be reachable before setup is complete
    // so the Playwright fixture can seed accounts and mark setup done.
    // These routes only exist when the `test-login` feature is enabled
    // (never in production builds).
    "/api/test",
];

fn is_allowed(uri: &Uri) -> bool {
    let path = uri.path();
    ALLOWED_PREFIXES
        .iter()
        .any(|p| path == *p || path.starts_with(&format!("{p}/")))
}

pub async fn setup_redirect(
    State(state): State<AppState>,
    req: Request<axum::body::Body>,
    next: Next,
) -> Response {
    if is_allowed(req.uri()) {
        return next.run(req).await;
    }

    match setup::is_setup_complete(&state.db).await {
        Ok(true) => next.run(req).await,
        Ok(false) => {
            // For HTML navigation we redirect; for JSON API consumers we
            // return 503 with a descriptive body so the client can
            // handle setup mode explicitly.
            let path = req.uri().path();
            if path.starts_with("/api/") {
                (
                    StatusCode::SERVICE_UNAVAILABLE,
                    "setup not complete; visit /setup",
                )
                    .into_response()
            } else {
                Redirect::to("/setup").into_response()
            }
        }
        Err(_) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}
