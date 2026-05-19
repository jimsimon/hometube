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
    usage_limit::enforce_usage_limit,
};
use crate::state::AppState;

pub mod accounts;
pub mod activity;
pub mod allowlist;
pub mod auth;
pub mod blocked;
pub mod bookmarks;
pub mod cache;
pub mod channels;
pub mod child_settings;
pub mod cron;
pub mod downloads;
pub mod family;
pub mod family_playlists;
pub mod feed;
pub mod likes;
pub mod notifications;
pub mod notifications_config;
pub mod pages;
pub mod playlists;
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
        // Parental preview (Phase 16)
        .route("/api/preview/video/{video_id}", get(preview::preview_video))
        .route(
            "/api/preview/channel/{channel_id}",
            get(preview::preview_channel),
        )
        .route(
            "/api/preview/playlist/{playlist_id}",
            get(preview::preview_playlist),
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
    // Family playlists (Phase 18). Both parents and children hit these
    // endpoints — the handlers themselves enforce the role-specific
    // rules (children only see assigned playlists; mutations are
    // parent-only).
    // -----------------------------------------------------------------
    let family_playlist_routes = Router::new()
        .route(
            "/api/family-playlists",
            get(family_playlists::list).post(family_playlists::create),
        )
        .route(
            "/api/family-playlists/{id}",
            get(family_playlists::detail)
                .put(family_playlists::update)
                .delete(family_playlists::delete),
        )
        .route(
            "/api/family-playlists/{id}/videos",
            post(family_playlists::add_video),
        )
        .route(
            "/api/family-playlists/{id}/videos/reorder",
            put(family_playlists::reorder),
        )
        .route(
            "/api/family-playlists/{id}/videos/{video_id}",
            delete_route(family_playlists::remove_video),
        );

    // -----------------------------------------------------------------
    // Video proxy + playback. Open to both parents and children, but
    // gated through the usage-limit middleware for children. Access
    // control (allowlist) is enforced inside each handler via
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
        .merge(proxy_routes)
        .route_layer(from_fn_with_state(state.clone(), enforce_usage_limit));

    // -----------------------------------------------------------------
    // Child feed + heartbeat. Used by the child home page and the
    // player. Gated by `require_child` so the handlers don't have to
    // re-check the role inline.
    // -----------------------------------------------------------------
    let child_routes = Router::new()
        .route("/api/feed/continue-watching", get(feed::continue_watching))
        .route("/api/feed/new-videos", get(feed::new_videos))
        .route("/api/feed/up-next", get(feed::up_next))
        .route("/api/usage/heartbeat", post(usage::heartbeat))
        .route("/api/search", get(search::child_search))
        .route_layer(axum::middleware::from_fn(require_child));

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
        .route("/api/playlists/{id}/videos", post(playlists::add_video))
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
        .route("/setup/pin", get(pages::set_pin))
        .route("/login", get(pages::login))
        .route("/profiles", get(pages::profile_picker))
        .route("/parent/home", get(pages::parent_home))
        .route("/parent/family", get(pages::parent_family))
        .route("/parent/system", get(pages::parent_system))
        .route("/parent/activity", get(pages::parent_activity))
        .route("/parent/playlists", get(pages::parent_playlists))
        .route("/parent/playlist/{id}", get(pages::parent_playlist))
        .route("/parent/preview/{kind}/{id}", get(pages::parent_preview))
        .route("/child/home", get(pages::child_home))
        .route("/child/channels", get(pages::child_channels))
        .route("/child/channel/{channel_id}", get(pages::child_channel))
        .route("/child/playlists", get(pages::child_playlists))
        .route("/child/playlist/{id}", get(pages::child_playlist))
        .route("/child/bookmarks", get(pages::child_bookmarks))
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
        .route("/api/test/reset", post(test_login::reset));

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
        .merge(auth_routes)
        .merge(setup_routes)
        .merge(parent_only)
        .merge(family_playlist_routes)
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
