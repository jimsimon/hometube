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
    usage_limit::enforce_usage_limit,
};
use crate::state::AppState;

pub mod accounts;
pub mod allowlist;
pub mod auth;
pub mod blocked;
pub mod child_settings;
pub mod feed;
pub mod pages;
pub mod search;
pub mod setup;
pub mod usage;
pub mod videos;

/// Build the top-level Axum router.
pub fn router(state: AppState) -> Router {
    let static_dir = state.config.static_dir.clone();

    // -----------------------------------------------------------------
    // Sub-router: parent-only APIs (account mgmt, allowlist, blocklist,
    // child settings, parent-side YouTube search).
    // -----------------------------------------------------------------
    let parent_only = Router::new()
        // Accounts
        .route("/api/accounts", get(accounts::list))
        .route(
            "/api/accounts/{id}",
            get(accounts::get).put(accounts::update).delete(accounts::delete),
        )
        // Parent-side YouTube search.
        .route("/api/parent/search", get(search::parent_search))
        // Allowlist: channels
        .route(
            "/api/children/{id}/allowlist/channels",
            get(allowlist::list_channels).post(allowlist::add_channel),
        )
        .route(
            "/api/children/{id}/allowlist/channels/{channel_id}",
            axum::routing::delete(allowlist::delete_channel),
        )
        // Allowlist: playlists
        .route(
            "/api/children/{id}/allowlist/playlists",
            get(allowlist::list_playlists).post(allowlist::add_playlist),
        )
        .route(
            "/api/children/{id}/allowlist/playlists/{playlist_id}",
            axum::routing::delete(allowlist::delete_playlist),
        )
        // Allowlist: videos
        .route(
            "/api/children/{id}/allowlist/videos",
            get(allowlist::list_videos).post(allowlist::add_video),
        )
        .route(
            "/api/children/{id}/allowlist/videos/{video_id}",
            axum::routing::delete(allowlist::delete_video),
        )
        // Blocked videos
        .route(
            "/api/children/{id}/blocked",
            get(blocked::list).post(blocked::add),
        )
        .route(
            "/api/children/{id}/blocked/{video_id}",
            axum::routing::delete(blocked::delete),
        )
        // Child settings + usage limits + stats
        .route(
            "/api/children/{id}/settings",
            get(child_settings::get_settings).put(child_settings::update_settings),
        )
        .route(
            "/api/children/{id}/usage-limits",
            get(child_settings::get_limits).put(child_settings::update_limits),
        )
        .route(
            "/api/children/{id}/usage-stats",
            get(child_settings::usage_stats),
        )
        .route_layer(axum::middleware::from_fn(require_parent));

    // -----------------------------------------------------------------
    // Video proxy + playback. Open to both parents and children, but
    // gated through the usage-limit middleware for children. Access
    // control (allowlist) is enforced inside each handler via
    // [`crate::services::access::can_child_view`].
    // -----------------------------------------------------------------
    let video_routes = Router::new()
        .route("/api/videos/{video_id}", get(videos::get_metadata))
        .route(
            "/api/videos/{video_id}/stream",
            get(videos::get_stream),
        )
        .route(
            "/api/videos/{video_id}/captions",
            get(videos::list_captions),
        )
        .route(
            "/api/videos/{video_id}/captions/{lang}",
            get(videos::get_caption),
        )
        .route("/api/proxy/segment", get(videos::get_segment))
        .route("/api/proxy/audio", get(videos::get_audio))
        .route(
            "/api/proxy/thumbnail/{video_id}",
            get(videos::get_thumbnail),
        )
        .route_layer(from_fn_with_state(state.clone(), enforce_usage_limit));

    // -----------------------------------------------------------------
    // Child feed + heartbeat. Used by the child home page and the
    // player.
    // -----------------------------------------------------------------
    let child_routes = Router::new()
        .route(
            "/api/feed/continue-watching",
            get(feed::continue_watching),
        )
        .route("/api/feed/new-videos", get(feed::new_videos))
        .route("/api/usage/heartbeat", post(usage::heartbeat));

    // -----------------------------------------------------------------
    // Sub-router: auth + setup
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
    // HTML pages
    // -----------------------------------------------------------------
    let page_routes = Router::new()
        .route("/", get(pages::root_or_setup))
        .route("/setup", get(pages::setup_wizard))
        .route("/parent/home", get(pages::parent_home))
        .route("/child/home", get(pages::child_home));

    // -----------------------------------------------------------------
    // Top-level router
    // -----------------------------------------------------------------
    Router::new()
        .merge(page_routes)
        .route("/api/health", get(health))
        .nest_service("/assets", ServeDir::new(static_dir))
        .merge(auth_routes)
        .merge(setup_routes)
        .merge(parent_only)
        .merge(video_routes)
        .merge(child_routes)
        .with_state(state.clone())
        .layer(from_fn_with_state(state.clone(), setup_redirect))
        .layer(from_fn_with_state(state, session_layer))
        .layer(CookieManagerLayer::new())
        .layer(CompressionLayer::new())
        .layer(TraceLayer::new_for_http())
}

async fn health() -> &'static str {
    "ok"
}
