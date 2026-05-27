//! Shared test harness.
//!
//! Boots an in-memory HomeTube app with no real external dependencies.
//! The harness exposes [`boot`] (zero-account sandbox, useful for the
//! setup-flow tests) and [`boot_setup_complete`] (a fully provisioned
//! app with one parent + one child, plus a signed session cookie for
//! the requested role).
//!
//! ## Cookie signing
//!
//! HomeTube signs every session cookie with the application's master
//! key. To avoid going through any auth flow, the harness signs the
//! session cookie itself using the same `Key` the app was built with,
//! then drops the resulting signed cookie value into
//! `axum_test::TestServer`'s jar. From the server's perspective this is
//! indistinguishable from a real cookie returned by `/api/auth/switch`.
//!
//! ## Why the test files all share this single module
//!
//! Cargo's `tests/` integration model compiles every top-level file in
//! `tests/` as its own binary. Files in `tests/common/` are *not*
//! compiled directly; they're pulled in via `mod common;` from each
//! test binary. That keeps the harness DRY without producing an empty
//! `common` test binary.

#![allow(dead_code)]

use axum_test::{TestServer, TestServerConfig};
use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
use sqlx::SqlitePool;
use std::str::FromStr;
use tower_cookies::cookie::{Cookie, CookieJar, Key};
use tower_cookies::Key as TowerKey;

use hometube::config::Config;
use hometube::middleware::auth::SESSION_COOKIE;
use hometube::models::account::AccountType;
use hometube::routes::build_router;
use hometube::services::setup::{set_config_value, KEY_SETUP_COMPLETE};
use hometube::state::AppState;

/// The fully-booted app under test plus the connection pool the test
/// can use for direct fixture inserts/asserts.
pub struct TestApp {
    pub server: TestServer,
    pub pool: SqlitePool,
    /// Master cookie key used by the app — exposed so tests that need
    /// to mint additional signed cookies can do so.
    pub key: TowerKey,
    /// IDs of any seeded accounts (`parent_id`, `child_id`). Both are
    /// `None` for [`boot`]; `boot_setup_complete` populates the role
    /// it was asked to provide and any peers it had to seed first.
    pub parent_id: Option<i64>,
    pub child_id: Option<i64>,
    /// Per-test temp directory used as `cache_dir`. Automatically cleaned
    /// up when the `TestApp` is dropped.
    pub cache_dir: tempfile::TempDir,
}

/// A signed `hometube_session` cookie value, ready to be added to the
/// test server's jar.
pub struct AuthCookie {
    pub name: &'static str,
    pub value: String,
    pub session_id: String,
    pub account_id: i64,
}

/// Boot a fresh, empty app with an in-memory SQLite database.
///
/// `setup_complete` is `false`; no accounts exist; cookies are signed
/// with a freshly generated [`Key`].
pub async fn boot() -> TestApp {
    let pool = make_in_memory_pool().await;
    hometube::db::migrate(&pool).await.expect("migrations");

    // Seed a deterministic cookie key so tests that reuse the same
    // signing key across multiple `TestApp` instances stay reproducible.
    let key_bytes = test_key_bytes();
    let cookie_key = TowerKey::from(&key_bytes[..]);

    let cache_dir = tempfile::tempdir().expect("create temp cache dir");

    let mut cfg = Config::from_env().expect("config");
    // Make sure no on-disk paths leak between tests.
    cfg.database_url = "sqlite::memory:".to_string();
    cfg.static_dir = "./frontend/dist".to_string();
    cfg.cache_dir = cache_dir.path().to_str().unwrap().to_string();

    // Point yt-dlp cookies to a writable temp location so tests that
    // exercise the cookies API don't fail on missing `/data/`.
    ensure_writable_cookies_path();

    // Also seed the proxy HMAC secret — code paths that look it up
    // shouldn't have to mutate state during a read-only test.
    seed_proxy_secret(&pool).await;

    let state = AppState::new(cfg, pool.clone(), cookie_key.clone());
    let app = build_router(state);

    let server = TestServer::new_with_config(
        app,
        TestServerConfig {
            save_cookies: true,
            ..TestServerConfig::default()
        },
    );

    TestApp {
        server,
        pool,
        key: cookie_key,
        parent_id: None,
        child_id: None,
        cache_dir,
    }
}

/// Boot the app with a completed setup: one parent (and one child if
/// `role == Child`), and a signed session cookie for the requested role
/// pre-installed in the test server's jar.
pub async fn boot_setup_complete(role: AccountType) -> (TestApp, AuthCookie) {
    let mut app = boot().await;

    set_config_value(&app.pool, KEY_SETUP_COMPLETE, "true")
        .await
        .expect("setup_complete");

    let parent_id = insert_account(&app.pool, "Parent One", AccountType::Parent).await;
    app.parent_id = Some(parent_id);

    let child_id = if matches!(role, AccountType::Child) {
        let id = insert_account(&app.pool, "Child One", AccountType::Child).await;
        app.child_id = Some(id);
        id
    } else {
        parent_id
    };

    let target_account_id = match role {
        AccountType::Parent => parent_id,
        AccountType::Child => child_id,
    };

    let auth = mint_session_cookie(&app, target_account_id).await;
    app.server
        .add_cookie(Cookie::new(auth.name, auth.value.clone()));
    (app, auth)
}

/// Variant of [`boot_setup_complete`] that seeds *both* a parent and a
/// child regardless of which role is asked for. Used by the parent /
/// child gating tests that need to assert behaviour from both sides.
pub async fn boot_with_parent_and_child(role: AccountType) -> (TestApp, AuthCookie) {
    let mut app = boot().await;

    set_config_value(&app.pool, KEY_SETUP_COMPLETE, "true")
        .await
        .expect("setup_complete");

    let parent_id = insert_account(&app.pool, "Parent One", AccountType::Parent).await;
    let child_id = insert_account(&app.pool, "Child One", AccountType::Child).await;
    app.parent_id = Some(parent_id);
    app.child_id = Some(child_id);

    let target = match role {
        AccountType::Parent => parent_id,
        AccountType::Child => child_id,
    };
    let auth = mint_session_cookie(&app, target).await;
    app.server
        .add_cookie(Cookie::new(auth.name, auth.value.clone()));
    (app, auth)
}

/// Insert a fully-populated `accounts` row. Returns the new `accounts.id`.
///
/// Delegates to [`hometube::models::account::insert_local`] so test
/// fixtures share the exact column-default behaviour the live signup
/// flow uses. A hand-rolled `INSERT` would diverge from production if
/// `accounts` ever grows a NOT NULL column with no default (the
/// production helper will be updated atomically; the test helper
/// would silently fall behind).
pub async fn insert_account(
    pool: &SqlitePool,
    display_name: &str,
    account_type: AccountType,
) -> i64 {
    hometube::models::account::insert_local(pool, display_name, None, account_type)
        .await
        .expect("insert account")
}

/// Insert a session row for `account_id` and return a signed cookie
/// representing it. The cookie is signed with the same `Key` the app
/// was built with so the server's cookie middleware will verify it.
pub async fn mint_session_cookie(app: &TestApp, account_id: i64) -> AuthCookie {
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
        .execute(&app.pool)
        .await
        .expect("insert session");

    // tower-cookies' Key wraps the same `cookie::Key`, but the public
    // API doesn't let us extract the raw bytes. We rebuild the
    // equivalent signing key from the deterministic test seed instead.
    let raw_key = Key::from(&test_key_bytes());
    let mut jar = CookieJar::new();
    jar.signed_mut(&raw_key)
        .add(Cookie::new(SESSION_COOKIE, session_id.clone()));
    let signed = jar.get(SESSION_COOKIE).expect("signed cookie").clone();

    AuthCookie {
        name: SESSION_COOKIE,
        value: signed.value().to_string(),
        session_id,
        account_id,
    }
}

/// Stable 64-byte signing key used by the harness. Anything 64+ bytes
/// works — this string is just convenient and doesn't appear anywhere
/// else in the codebase, so a leak via failing assertions is obvious.
pub fn test_key_bytes() -> [u8; 64] {
    *b"hometube-tests-deterministic-cookie-signing-key-64-bytes-aaaaaaaa"
        .first_chunk::<64>()
        .unwrap()
}

/// Build a fresh in-memory SQLite pool. Each test gets its own private
/// database — the `:memory:` URI plus `cache=private` ensures
/// connections within the same pool can share state, while two
/// independent pools (i.e., two tests running in parallel) cannot see
/// each other's writes.
///
/// We allow up to 8 connections to match production. SQLite's
/// in-memory database needs the `cache=shared` plus a stable name
/// (here, the pool's own random suffix via `?mode=memory&cache=shared`)
/// for that to work cleanly without each connection getting its own
/// empty database. We pick a per-pool unique URI so parallel tests
/// don't collide.
pub async fn make_in_memory_pool() -> SqlitePool {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nonce = COUNTER.fetch_add(1, Ordering::SeqCst);
    let url = format!("file:hometube-test-{nonce}?mode=memory&cache=shared");
    let opts = SqliteConnectOptions::from_str(&url)
        .expect("opts")
        .create_if_missing(true)
        .foreign_keys(true);

    SqlitePoolOptions::new()
        .max_connections(8)
        // Tell the pool to keep at least one connection alive — this
        // guarantees the in-memory database (which is destroyed when
        // the last connection closes) survives between requests.
        .min_connections(1)
        .connect_with(opts)
        .await
        .expect("connect")
}

/// Seed the proxy HMAC secret so dash signing routines that read it
/// don't have to mutate state during a test.
pub async fn seed_proxy_secret(pool: &SqlitePool) {
    use base64::Engine;
    let bytes = [7u8; 32];
    let encoded = base64::engine::general_purpose::STANDARD.encode(bytes);
    set_config_value(pool, "proxy_hmac_secret", &encoded)
        .await
        .expect("seed proxy secret");
}

// ---------------------------------------------------------------------------
// Schema-aware fixture helpers (migrations 024 + 025 normalisation)
// ---------------------------------------------------------------------------
//
// After migrations 024 / 025 the per-child tables (`allowlisted_videos`,
// `blocked_videos`, `hidden_videos`, `watch_history`, `video_likes`,
// `offline_downloads`) are FK-only — title / thumbnail / duration /
// channel_id live on the canonical `videos` table. `allowlisted_channels`
// is similarly FK-only into the `channels` reference table.
//
// These helpers seed both sides at once so tests can keep the
// "insert a video the child can see" semantics they had before the
// normalisation, without each test re-implementing the upsert dance.

/// Seed (or refresh) a `videos` row. Delegates to the production
/// [`hometube::models::video::upsert`] helper so test fixtures stay in
/// sync with the route handlers' canonical upsert semantics (placeholder
/// title fallback, NULLIF guards on the conflict path, etc.).
pub async fn seed_video(
    pool: &SqlitePool,
    video_id: &str,
    title: Option<&str>,
    channel_id: Option<&str>,
) {
    seed_video_full(pool, video_id, title, channel_id, None, None).await;
}

/// Full-shape variant of [`seed_video`] used by the per-table seed
/// helpers below — they all funnel through here so the per-test row
/// shape is irrelevant to the production helper, which sees exactly
/// the same arguments it would in a route.
async fn seed_video_full(
    pool: &SqlitePool,
    video_id: &str,
    title: Option<&str>,
    channel_id: Option<&str>,
    duration_seconds: Option<i64>,
    thumbnail_url: Option<&str>,
) {
    hometube::models::video::upsert(
        pool,
        video_id,
        title,
        channel_id,
        duration_seconds,
        thumbnail_url,
    )
    .await
    .expect("seed videos row");
}

/// Seed a `channels` row with a title so allowlisted-channel surfaces
/// have something to render.
///
/// Delegates to the production
/// [`hometube::services::feed_cache::upsert_channel_with_metadata`]
/// helper so test fixtures share the exact `NULLIF`-guarded rename-
/// propagation semantics route handlers see. A hand-rolled
/// `COALESCE(excluded, stored)` would diverge from production on
/// `Some("")` inputs (production treats `""` as missing; the
/// hand-rolled form would persist it).
pub async fn seed_channel(pool: &SqlitePool, channel_id: &str, title: Option<&str>) {
    hometube::services::feed_cache::upsert_channel_with_metadata(
        pool, channel_id, title, None, None,
    )
    .await
    .expect("seed channels row");
}

/// Insert a `videos` row plus the per-child `allowlisted_videos`
/// linkage. Convenience for tests that previously did the denormalised
/// insert directly.
pub async fn allowlist_video(
    pool: &SqlitePool,
    child_id: i64,
    added_by: i64,
    video_id: &str,
    title: Option<&str>,
    channel_id: Option<&str>,
) {
    seed_video(pool, video_id, title, channel_id).await;
    sqlx::query(
        "INSERT INTO allowlisted_videos (child_account_id, video_id, added_by) \
         VALUES (?, ?, ?) \
         ON CONFLICT(child_account_id, video_id) DO NOTHING",
    )
    .bind(child_id)
    .bind(video_id)
    .bind(added_by)
    .execute(pool)
    .await
    .expect("seed allowlisted_videos row");
}

/// Insert a `channels` row plus the per-child `allowlisted_channels`
/// linkage.
pub async fn allowlist_channel(
    pool: &SqlitePool,
    child_id: i64,
    added_by: i64,
    channel_id: &str,
    title: Option<&str>,
) {
    seed_channel(pool, channel_id, title).await;
    sqlx::query(
        "INSERT INTO allowlisted_channels (child_account_id, channel_id, added_by) \
         VALUES (?, ?, ?) \
         ON CONFLICT(child_account_id, channel_id) DO NOTHING",
    )
    .bind(child_id)
    .bind(channel_id)
    .bind(added_by)
    .execute(pool)
    .await
    .expect("seed allowlisted_channels row");
}

/// Backwards-compat shim: many existing tests passed a denormalised
/// row shape (video_id, video_title, video_thumbnail_url, channel_title)
/// to the per-child tables that migrations 024 / 025 made FK-only.
/// This helper splits the input across `videos` + `channels` + the
/// target per-child table so those tests keep working without each one
/// having to learn the new schema.
///
/// The positional-argument shape mirrors the pre-normalisation row
/// layout that ~30 existing call sites use; switching to a struct
/// builder would balloon the diff for no real readability win in tests.
#[allow(clippy::too_many_arguments)]
pub async fn seed_hidden(
    pool: &SqlitePool,
    child_id: i64,
    video_id: &str,
    video_title: Option<&str>,
    channel_id: Option<&str>,
    channel_title: Option<&str>,
    video_thumbnail_url: Option<&str>,
    duration_seconds: Option<i64>,
) {
    if let Some(cid) = channel_id {
        seed_channel(pool, cid, channel_title).await;
    }
    seed_video_full(
        pool,
        video_id,
        video_title,
        channel_id,
        duration_seconds,
        video_thumbnail_url,
    )
    .await;
    sqlx::query(
        "INSERT INTO hidden_videos (child_account_id, video_id) VALUES (?, ?) \
         ON CONFLICT(child_account_id, video_id) DO NOTHING",
    )
    .bind(child_id)
    .bind(video_id)
    .execute(pool)
    .await
    .expect("seed hidden_videos");
}

/// Seed a row into `video_likes` plus the canonical `videos` /
/// `channels` rows the likes JOIN now requires.
///
/// See `seed_hidden` for why the positional-argument shape is kept.
#[allow(clippy::too_many_arguments)]
pub async fn seed_like(
    pool: &SqlitePool,
    child_id: i64,
    video_id: &str,
    video_title: Option<&str>,
    channel_id: Option<&str>,
    channel_title: Option<&str>,
    video_thumbnail_url: Option<&str>,
    duration_seconds: Option<i64>,
) {
    if let Some(cid) = channel_id {
        seed_channel(pool, cid, channel_title).await;
    }
    seed_video_full(
        pool,
        video_id,
        video_title,
        channel_id,
        duration_seconds,
        video_thumbnail_url,
    )
    .await;
    sqlx::query(
        "INSERT INTO video_likes (child_account_id, video_id, is_deleted) \
         VALUES (?, ?, 0) \
         ON CONFLICT(child_account_id, video_id) DO UPDATE SET is_deleted = 0, \
            updated_at = unixepoch()",
    )
    .bind(child_id)
    .bind(video_id)
    .execute(pool)
    .await
    .expect("seed video_likes");
}

/// Seed a row into `watch_history` plus the canonical `videos` row.
///
/// See `seed_hidden` for why the positional-argument shape is kept.
#[allow(clippy::too_many_arguments)]
pub async fn seed_watch_history(
    pool: &SqlitePool,
    child_id: i64,
    video_id: &str,
    video_title: Option<&str>,
    channel_id: Option<&str>,
    channel_title: Option<&str>,
    video_thumbnail_url: Option<&str>,
    duration_seconds: Option<i64>,
    progress_seconds: i64,
    last_watched_at: Option<i64>,
) {
    if let Some(cid) = channel_id {
        seed_channel(pool, cid, channel_title).await;
    }
    seed_video_full(
        pool,
        video_id,
        video_title,
        channel_id,
        duration_seconds,
        video_thumbnail_url,
    )
    .await;
    let watched = last_watched_at.unwrap_or_else(|| chrono::Utc::now().timestamp());
    sqlx::query(
        "INSERT INTO watch_history (child_account_id, video_id, progress_seconds, last_watched_at) \
         VALUES (?, ?, ?, ?) \
         ON CONFLICT(child_account_id, video_id) DO UPDATE SET \
            progress_seconds = excluded.progress_seconds, \
            last_watched_at = excluded.last_watched_at",
    )
    .bind(child_id)
    .bind(video_id)
    .bind(progress_seconds)
    .bind(watched)
    .execute(pool)
    .await
    .expect("seed watch_history");
}

/// Seed a row into `blocked_videos` plus the canonical `videos` row.
pub async fn seed_blocked(
    pool: &SqlitePool,
    child_id: i64,
    blocked_by: i64,
    video_id: &str,
    video_title: Option<&str>,
) {
    seed_video_full(pool, video_id, video_title, None, None, None).await;
    sqlx::query(
        "INSERT INTO blocked_videos (child_account_id, video_id, blocked_by) \
         VALUES (?, ?, ?) \
         ON CONFLICT(child_account_id, video_id) DO NOTHING",
    )
    .bind(child_id)
    .bind(video_id)
    .bind(blocked_by)
    .execute(pool)
    .await
    .expect("seed blocked_videos");
}

/// Seed a row into `offline_downloads` plus the canonical `videos` row.
///
/// See `seed_hidden` for why the positional-argument shape is kept.
#[allow(clippy::too_many_arguments)]
pub async fn seed_offline_download(
    pool: &SqlitePool,
    child_id: i64,
    video_id: &str,
    video_title: Option<&str>,
    channel_id: Option<&str>,
    channel_title: Option<&str>,
    video_thumbnail_url: Option<&str>,
    quality_label: &str,
    status: &str,
) {
    if let Some(cid) = channel_id {
        seed_channel(pool, cid, channel_title).await;
    }
    seed_video_full(
        pool,
        video_id,
        video_title,
        channel_id,
        None,
        video_thumbnail_url,
    )
    .await;
    sqlx::query(
        "INSERT INTO offline_downloads (child_account_id, video_id, quality_label, status) \
         VALUES (?, ?, ?, ?) \
         ON CONFLICT(child_account_id, video_id, quality_label) DO UPDATE SET status = excluded.status",
    )
    .bind(child_id)
    .bind(video_id)
    .bind(quality_label)
    .bind(status)
    .execute(pool)
    .await
    .expect("seed offline_downloads");
}

/// Insert a `channel_videos` archive row. Seeds the parent `channels`
/// + `videos` rows as needed.
pub async fn seed_channel_video(
    pool: &SqlitePool,
    channel_id: &str,
    channel_title: Option<&str>,
    video_id: &str,
    title: Option<&str>,
    published_at: Option<i64>,
    source: &str,
) {
    seed_channel(pool, channel_id, channel_title).await;
    seed_video(pool, video_id, title, Some(channel_id)).await;
    let now = published_at.unwrap_or_else(|| chrono::Utc::now().timestamp());
    sqlx::query(
        "INSERT INTO channel_videos \
            (channel_id, video_id, published_at, first_seen_at, last_seen_at, source) \
         VALUES (?, ?, ?, ?, ?, ?) \
         ON CONFLICT(channel_id, video_id) DO NOTHING",
    )
    .bind(channel_id)
    .bind(video_id)
    .bind(published_at)
    .bind(now)
    .bind(now)
    .bind(source)
    .execute(pool)
    .await
    .expect("seed channel_videos");
}

/// Ensure `YTDLP_COOKIES_PATH` points to a writable temp location.
/// Called once per `boot()` invocation; the env var is process-wide and
/// idempotent across parallel tests within the same test binary.
fn ensure_writable_cookies_path() {
    use std::sync::Once;
    static INIT: Once = Once::new();
    INIT.call_once(|| {
        let path = std::env::temp_dir().join("hometube-test-cookies.txt");
        unsafe { std::env::set_var("YTDLP_COOKIES_PATH", path.to_str().unwrap()) };
    });
}
