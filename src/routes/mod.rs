//! HTTP routing.
//!
//! [`router`] composes all route modules into a single Axum [`Router`]. As
//! later phases add features, each gets its own submodule and is mounted
//! here.

use axum::{routing::get, Router};
use tower_http::{compression::CompressionLayer, services::ServeDir, trace::TraceLayer};

use crate::state::AppState;

pub mod pages;

/// Build the top-level Axum router.
pub fn router(state: AppState) -> Router {
    let static_dir = state.config.static_dir.clone();

    Router::new()
        // HTML pages.
        .route("/", get(pages::home))
        // Liveness probe (used by Docker healthcheck).
        .route("/api/health", get(health))
        // Vite-built JS/CSS bundles.
        .nest_service("/assets", ServeDir::new(static_dir))
        .with_state(state)
        .layer(CompressionLayer::new())
        .layer(TraceLayer::new_for_http())
}

async fn health() -> &'static str {
    "ok"
}
