//! HTTP routing.
//!
//! [`router`] composes all route modules into a single Axum [`Router`].
//! Layers are applied in the order documented inline so that:
//!
//! 1. `tower-cookies` parses incoming cookies,
//! 2. the session middleware resolves the session ID into a
//!    [`crate::middleware::auth::CurrentAccount`] extension,
//! 3. the setup-redirect middleware bounces unauthenticated users to
//!    `/setup` until installation is finished,
//! 4. compression + tracing wrap everything.
//!
//! Parent-only / child-only gates are applied at the sub-router level.

use axum::middleware::from_fn_with_state;
use axum::{
    routing::{get, post, put},
    Router,
};
use tower_cookies::CookieManagerLayer;
use tower_http::{compression::CompressionLayer, services::ServeDir, trace::TraceLayer};

use crate::middleware::{
    account_type::require_parent, auth::session_layer, setup_redirect::setup_redirect,
};
use crate::state::AppState;

pub mod accounts;
pub mod auth;
pub mod pages;
pub mod setup;

/// Build the top-level Axum router.
pub fn router(state: AppState) -> Router {
    let static_dir = state.config.static_dir.clone();

    // -----------------------------------------------------------------
    // Sub-router: account management (parent-only)
    // -----------------------------------------------------------------
    let parent_only = Router::new()
        .route("/api/accounts", get(accounts::list))
        .route(
            "/api/accounts/{id}",
            get(accounts::get).put(accounts::update).delete(accounts::delete),
        )
        .route_layer(axum::middleware::from_fn(require_parent));

    // -----------------------------------------------------------------
    // Sub-router: auth + setup (no role gate; auth handles its own
    // lifecycle, setup must be reachable before any account exists)
    // -----------------------------------------------------------------
    let auth_routes = Router::new()
        .route("/api/auth/login", get(auth::login))
        .route("/api/auth/callback", get(auth::callback))
        .route("/api/auth/logout", post(auth::logout))
        .route("/api/auth/me", get(auth::me))
        .route("/api/auth/profiles", get(auth::profiles))
        .route("/api/auth/switch", post(auth::switch))
        .route("/api/auth/pin", put(auth::set_pin));

    let setup_routes = Router::new()
        .route("/api/setup/status", get(setup::status))
        .route("/api/setup/credentials", post(setup::save_credentials))
        .route("/api/setup/test-credentials", post(setup::test_credentials))
        .route("/api/setup/complete", post(setup::complete));

    // -----------------------------------------------------------------
    // Top-level router
    // -----------------------------------------------------------------
    Router::new()
        // HTML pages
        .route("/", get(pages::root_or_setup))
        .route("/setup", get(pages::setup_wizard))
        // Liveness probe (used by Docker healthcheck).
        .route("/api/health", get(health))
        // Vite-built JS/CSS bundles.
        .nest_service("/assets", ServeDir::new(static_dir))
        // Merge all the routers above. The DELETE-first ordering avoids
        // colliding with `/api/accounts/{id}`'s GET/PUT.
        .merge(auth_routes)
        .merge(setup_routes)
        .merge(parent_only)
        .with_state(state.clone())
        // The setup-redirect middleware reads from the DB, so it needs
        // state.
        .layer(from_fn_with_state(state.clone(), setup_redirect))
        // Session middleware likewise needs state (for the cookie key).
        .layer(from_fn_with_state(state, session_layer))
        // Cookies must be parsed before either of the middlewares above.
        .layer(CookieManagerLayer::new())
        .layer(CompressionLayer::new())
        .layer(TraceLayer::new_for_http())
}

async fn health() -> &'static str {
    "ok"
}
