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

use axum::extract::State;
use axum::middleware::from_fn_with_state;
use axum::{
    routing::{delete as delete_route, get, post, put},
    Router,
};
use std::path::PathBuf;
use tower_cookies::CookieManagerLayer;
use tower_http::{
    compression::CompressionLayer,
    services::{ServeDir, ServeFile},
    trace::TraceLayer,
};

use crate::middleware::{
    account_type::{require_child, require_parent},
    auth::session_layer,
    setup_redirect::setup_redirect,
};
use crate::state::AppState;

pub mod accounts;
pub mod activity;
pub mod allowlist;
pub mod auth;
pub mod blocked;
pub mod cache;
pub mod channels;
pub mod child_settings;
pub mod cron;
pub mod downloads;
pub mod family;
pub mod feed;
pub mod hidden;
pub mod likes;
pub mod notifications;
pub mod notifications_config;
pub mod pages;
pub mod preview;
pub mod search;
pub mod setup;
pub mod subscriptions;
pub mod system;
#[cfg(feature = "test-login")]
pub mod test_login;
pub mod timer;
pub mod usage;
pub mod videos;

/// Build the top-level Axum router.
///
/// Alias for [`router`] kept around so integration tests in `tests/`
/// can call it under the documented name.
pub fn build_router(state: AppState) -> Router {
    router(state)
}

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
            get(accounts::get)
                .put(accounts::update)
                .delete(accounts::delete),
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
        // Child settings
        .route(
            "/api/children/{id}/settings",
            get(child_settings::get_settings).put(child_settings::update_settings),
        )
        // Cron job management
        .route("/api/cron/jobs", get(cron::list_jobs))
        .route(
            "/api/cron/jobs/{id}",
            get(cron::get_job).put(cron::update_job),
        )
        .route("/api/cron/jobs/{id}/run", post(cron::run_now))
        .route("/api/cron/jobs/{id}/runs", get(cron::list_runs))
        // System / yt-dlp
        .route("/api/system/ytdlp", get(system::get_ytdlp))
        .route("/api/system/ytdlp/update", post(system::update_ytdlp))
        .route(
            "/api/system/ytdlp/cookies",
            get(system::get_cookies)
                .put(system::set_cookies)
                .delete(system::delete_cookies),
        )
        .route("/api/system/pot-server", get(system::get_pot_server_status))
        // Feed-cache diagnostics + refresher tunables (parent-only)
        .route("/api/admin/feed-sources", get(feed::admin_list_sources))
        .route(
            "/api/admin/feed-refresher/settings",
            get(feed::admin_get_refresher_settings).put(feed::admin_put_refresher_settings),
        )
        .route(
            "/api/admin/feed-refresher/capacity",
            get(feed::admin_get_refresher_capacity),
        )
        // Family management (Phase 13)
        .route(
            "/api/family/members",
            get(family::list_members).post(family::add_member),
        )
        .route(
            "/api/family/members/{id}",
            put(family::update_member).delete(family::delete_member),
        )
        // Cache management
        .route("/api/cache/stats", get(cache::stats))
        .route(
            "/api/cache/settings",
            get(cache::get_settings).put(cache::update_settings),
        )
        .route("/api/cache/videos", get(cache::list_videos))
        .route(
            "/api/cache/videos/{video_id}",
            axum::routing::delete(cache::delete_video),
        )
        .route("/api/cache/clear", post(cache::clear_all))
        .route("/api/cache/evictions", get(cache::recent_evictions))
        // Parental preview (Phase 16)
        .route("/api/preview/video/{video_id}", get(preview::preview_video))
        .route(
            "/api/preview/channel/{channel_id}",
            get(preview::preview_channel),
        )
        // Watch-activity dashboard (Phase 17)
        .route(
            "/api/children/{id}/activity/summary",
            get(activity::summary),
        )
        .route(
            "/api/children/{id}/activity/history",
            get(activity::history),
        )
        .route(
            "/api/children/{id}/activity/top-channels",
            get(activity::top_channels),
        )
        .route(
            "/api/children/{id}/activity/search-log",
            get(activity::search_log),
        )
        // Parent notifications (Phase 17)
        .route("/api/notifications", get(notifications::list))
        .route(
            "/api/notifications/unread-count",
            get(notifications::unread_count),
        )
        .route(
            "/api/notifications/read-all",
            put(notifications::mark_all_read),
        )
        .route(
            "/api/notifications/{id}/read",
            put(notifications::mark_read),
        )
        .route(
            "/api/notifications/{id}",
            axum::routing::delete(notifications::delete),
        )
        // External-notification forwarder (Apprise / ntfy.sh / Gotify)
        .route(
            "/api/notifications/config",
            get(notifications_config::get_config).put(notifications_config::put_config),
        )
        .route(
            "/api/notifications/config/test",
            post(notifications_config::test),
        )
        .route_layer(axum::middleware::from_fn(require_parent));

    // -----------------------------------------------------------------
    // Video proxy + playback. Open to both parents and children.
    // Access control (allowlist) is enforced inside each handler via
    // [`crate::services::access::can_child_view`].
    //
    // The proxy endpoints (format / thumbnail) additionally pass
    // through the per-account rate limiter to bound how aggressively
    // any one client can pull bytes through us.
    // -----------------------------------------------------------------
    let proxy_routes = Router::new()
        .route("/api/proxy/format", get(videos::get_format))
        .route(
            "/api/proxy/thumbnail/{video_id}",
            get(videos::get_thumbnail),
        )
        .route_layer(axum::middleware::from_fn(
            crate::middleware::rate_limit::rate_limit_proxies,
        ));

    let video_routes = Router::new()
        .route("/api/videos/{video_id}", get(videos::get_metadata))
        .route("/api/videos/{video_id}/stream", get(videos::get_stream))
        .route(
            "/api/videos/{video_id}/stream/manifest.mpd",
            get(videos::get_stream_manifest),
        )
        .route(
            "/api/videos/{video_id}/captions",
            get(videos::list_captions),
        )
        .route(
            "/api/videos/{video_id}/captions/{lang}",
            get(videos::get_caption),
        )
        .merge(proxy_routes);

    // -----------------------------------------------------------------
    // Child feed + heartbeat. Used by the child home page and the
    // player. Gated by `require_child` so the handlers don't have to
    // re-check the role inline.
    // -----------------------------------------------------------------
    let child_routes = Router::new()
        .route("/api/feed/continue-watching", get(feed::continue_watching))
        .route("/api/feed/watch-again", get(feed::watch_again))
        .route("/api/feed/new-videos", get(feed::new_videos))
        .route("/api/feed/up-next", get(feed::up_next))
        .route("/api/usage/heartbeat", post(usage::heartbeat))
        .route("/api/usage/progress", post(usage::progress))
        .route("/api/search", get(search::child_search))
        .route_layer(axum::middleware::from_fn(require_child));

    // -----------------------------------------------------------------
    // Child-only APIs: channels, subscriptions, likes, sleep timer,
    // and "my settings" read-only view.
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
        // Likes
        .route("/api/likes", get(likes::list))
        .route(
            "/api/likes/{video_id}",
            post(likes::like).delete(likes::unlike),
        )
        // Per-child hidden videos
        .route("/api/hidden", get(hidden::list).post(hidden::add))
        .route("/api/hidden/{video_id}", delete_route(hidden::remove))
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
        // Offline downloads (Phase 16). The actual storage lives in the
        // browser; the backend tracks state + hands out a stream URL.
        .route(
            "/api/downloads",
            get(downloads::list).post(downloads::create),
        )
        .route(
            "/api/downloads/{video_id}",
            put(downloads::update).delete(downloads::delete),
        )
        .route("/api/downloads/{video_id}/stream", get(downloads::stream))
        .route_layer(axum::middleware::from_fn(require_child));

    // -----------------------------------------------------------------
    // Sub-router: auth + setup
    // -----------------------------------------------------------------
    let auth_routes = Router::new()
        .route("/api/auth/register", post(auth::register))
        .route("/api/auth/logout", post(auth::logout))
        .route("/api/auth/me", get(auth::me))
        .route("/api/auth/profiles", get(auth::profiles))
        .route("/api/auth/switch", post(auth::switch))
        .route("/api/auth/pin", put(auth::set_pin));

    let setup_routes = Router::new()
        .route("/api/setup/status", get(setup::status))
        .route("/api/setup/complete", post(setup::complete));

    // -----------------------------------------------------------------
    // HTML pages.
    // -----------------------------------------------------------------
    let video_page = Router::new().route("/child/video/{video_id}", get(pages::child_video));

    let page_routes = Router::new()
        .route("/", get(pages::root_or_setup))
        .route("/setup", get(pages::setup_wizard))
        .route("/setup/pin", get(pages::set_pin))
        .route("/login", get(pages::login))
        .route("/profiles", get(pages::profile_picker))
        .route("/parent/home", get(pages::parent_home))
        .route("/parent/family", get(pages::parent_family))
        .route("/parent/system", get(pages::parent_system))
        .route("/parent/activity", get(pages::parent_activity))
        .route("/parent/preview/{kind}/{id}", get(pages::parent_preview))
        .route("/child/home", get(pages::child_home))
        .route("/child/channels", get(pages::child_channels))
        .route("/child/channel/{channel_id}", get(pages::child_channel))
        .route("/child/hidden", get(pages::child_hidden))
        .route("/child/downloads", get(pages::child_downloads))
        .route("/child/search", get(pages::child_search));

    // -----------------------------------------------------------------
    // Top-level router
    // -----------------------------------------------------------------
    // Service worker, web app manifest, and offline fallback page have
    // to live at the document root for browsers to honour them.
    let static_root = PathBuf::from(&static_dir);

    #[cfg(feature = "test-login")]
    let test_login_routes = Router::new()
        .route("/api/test/seed", post(test_login::seed))
        .route("/api/test/login-as", post(test_login::login_as))
        .route("/api/test/reset", post(test_login::reset))
        .route("/api/test/seed-feed-item", post(test_login::seed_feed_item));

    #[allow(unused_mut)]
    let mut router = Router::new()
        .merge(page_routes)
        .merge(video_page)
        .route("/api/health", get(health));

    #[cfg(feature = "test-login")]
    {
        router = router.merge(test_login_routes);
    }

    router
        .nest_service("/assets", ServeDir::new(&static_dir))
        .route_service("/sw.js", ServeFile::new(static_root.join("sw.js")))
        .route_service("/sw.js.map", ServeFile::new(static_root.join("sw.js.map")))
        .route_service(
            "/manifest.webmanifest",
            ServeFile::new(static_root.join("manifest.webmanifest")),
        )
        .route_service(
            "/offline.html",
            ServeFile::new(static_root.join("offline.html")),
        )
        .route_service(
            "/favicon.ico",
            ServeFile::new(static_root.join("favicon.ico")),
        )
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

/// `GET /api/health` — liveness probe used by the Docker `HEALTHCHECK`
/// directive. Returns 200 with `ok` when the server can still talk to
/// SQLite; 503 with a short message otherwise.
async fn health(
    State(state): State<AppState>,
) -> Result<&'static str, (axum::http::StatusCode, &'static str)> {
    match sqlx::query("SELECT 1").execute(&state.db).await {
        Ok(_) => Ok("ok"),
        Err(err) => {
            tracing::warn!(error = %err, "health check failed: db unreachable");
            Err((
                axum::http::StatusCode::SERVICE_UNAVAILABLE,
                "db unreachable",
            ))
        }
    }
}
