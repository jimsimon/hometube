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
    routing::{delete as delete_route, get, post, put},
    Router,
};
use tower_cookies::CookieManagerLayer;
use tower_http::{compression::CompressionLayer, services::ServeDir, trace::TraceLayer};

use crate::middleware::{
    account_type::{require_child, require_parent},
    auth::session_layer,
    setup_redirect::setup_redirect,
    usage_limit::enforce_usage_limit,
};
use crate::state::AppState;

pub mod accounts;
pub mod allowlist;
pub mod auth;
pub mod blocked;
pub mod bookmarks;
pub mod channels;
pub mod child_settings;
pub mod feed;
pub mod likes;
pub mod pages;
pub mod playlists;
pub mod search;
pub mod setup;
pub mod subscriptions;
pub mod timer;
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
        .route("/api/feed/up-next", get(feed::up_next))
        .route("/api/usage/heartbeat", post(usage::heartbeat));

    // -----------------------------------------------------------------
    // Child-only APIs: channels, subscriptions, playlists, likes,
    // bookmarks, sleep timer, and "my settings" read-only view.
    // -----------------------------------------------------------------
    let child_only = Router::new()
        // Channels
        .route("/api/channels/{channel_id}", get(channels::get_channel))
        .route(
            "/api/channels/{channel_id}/videos",
            get(channels::list_videos),
        )
        // Subscriptions
        .route(
            "/api/subscriptions",
            get(subscriptions::list).post(subscriptions::subscribe),
        )
        .route(
            "/api/subscriptions/{channel_id}",
            delete_route(subscriptions::unsubscribe),
        )
        // Playlists
        .route(
            "/api/playlists",
            get(playlists::list).post(playlists::create),
        )
        .route("/api/playlists/library", post(playlists::add_library))
        .route(
            "/api/playlists/{id}",
            get(playlists::detail)
                .put(playlists::update)
                .delete(playlists::delete),
        )
        .route(
            "/api/playlists/{id}/videos",
            post(playlists::add_video),
        )
        .route(
            "/api/playlists/{id}/videos/reorder",
            put(playlists::reorder_videos),
        )
        .route(
            "/api/playlists/{id}/videos/{video_id}",
            delete_route(playlists::remove_video),
        )
        // Likes
        .route("/api/likes", get(likes::list))
        .route(
            "/api/likes/{video_id}",
            post(likes::like).delete(likes::unlike),
        )
        // Bookmarks. Note that `/api/bookmarks/:videoId` (GET) and
        // `/api/bookmarks/:id` (PUT/DELETE) share the same path
        // pattern as far as axum's router is concerned — they're
        // distinguished by HTTP method, so we register them on a
        // single MethodRouter.
        .route(
            "/api/bookmarks",
            get(bookmarks::list).post(bookmarks::create),
        )
        .route(
            "/api/bookmarks/{id}",
            get(bookmarks::list_for_video)
                .put(bookmarks::update)
                .delete(bookmarks::delete),
        )
        // Sleep timer
        .route(
            "/api/timer",
            get(timer::get).post(timer::create).delete(timer::cancel),
        )
        // Current child's read-only settings view
        .route(
            "/api/children/me/settings",
            get(child_settings::get_my_settings),
        )
        .route_layer(axum::middleware::from_fn(require_child));

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
    // HTML pages. The /child/video/:id page is layered with the
    // usage-limit middleware (matching the plan's coordination notes)
    // so a child can't bypass the cap by deep-linking. On a 403 the
    // middleware returns JSON, which the browser displays as plain
    // text — by the time the player can issue an API call the
    // <hometube-usage-limit-overlay> takes over and shows a friendly
    // dialog.
    // -----------------------------------------------------------------
    let video_page = Router::new()
        .route("/child/video/{video_id}", get(pages::child_video))
        .route_layer(from_fn_with_state(state.clone(), enforce_usage_limit));

    let page_routes = Router::new()
        .route("/", get(pages::root_or_setup))
        .route("/setup", get(pages::setup_wizard))
        .route("/parent/home", get(pages::parent_home))
        .route("/child/home", get(pages::child_home))
        .route("/child/channels", get(pages::child_channels))
        .route("/child/channel/{channel_id}", get(pages::child_channel))
        .route("/child/playlists", get(pages::child_playlists))
        .route("/child/playlist/{id}", get(pages::child_playlist));

    // -----------------------------------------------------------------
    // Top-level router
    // -----------------------------------------------------------------
    Router::new()
        .merge(page_routes)
        .merge(video_page)
        .route("/api/health", get(health))
        .nest_service("/assets", ServeDir::new(static_dir))
        .merge(auth_routes)
        .merge(setup_routes)
        .merge(parent_only)
        .merge(video_routes)
        .merge(child_routes)
        .merge(child_only)
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
