//! Shared test harness.
//!
//! Boots an in-memory HomeTube app with no real external dependencies.
//! Google OAuth + the YouTube Data API are never reached because:
//!
//! - we never invoke routes that exchange OAuth codes (callbacks etc.),
//!   and the discovery probe in `POST /api/setup/credentials` is the
//!   only network call inside the routes we *do* test (we explicitly
//!   skip those happy-path tests).
//! - allowlist + cache test fixtures bypass the YouTube client by
//!   inserting rows directly into `allowlisted_*` and
//!   `video_metadata_cache`.
//!
//! The harness exposes [`boot`] (zero-account sandbox, useful for the
//! setup-flow tests) and [`boot_setup_complete`] (a fully provisioned
//! app with one parent + one child, plus a signed session cookie for
//! the requested role).
//!
//! ## Cookie signing
//!
//! HomeTube signs every session cookie with the application's master
//! key. To avoid going through the full OAuth dance, the harness signs
//! the session cookie itself using the same `Key` the app was built
//! with, then drops the resulting signed cookie value into
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
use hometube::services::setup::{
    set_config_value, KEY_GOOGLE_CLIENT_ID, KEY_GOOGLE_CLIENT_SECRET, KEY_GOOGLE_REDIRECT_URI,
    KEY_SETUP_COMPLETE, KEY_YOUTUBE_API_KEY,
};
use hometube::state::AppState;

/// The fully-booted app under test plus the connection pool the test
/// can use for direct fixture inserts/asserts.
pub struct TestApp {
    pub server: TestServer,
    pub pool: SqlitePool,
    /// Master cookie key used by the app — exposed so tests that need
    /// to mint additional signed cookies (e.g., the `hometube_oauth`
    /// flow cookie) can do so.
    pub key: TowerKey,
    /// IDs of any seeded accounts (`parent_id`, `child_id`). Both are
    /// `None` for [`boot`]; `boot_setup_complete` populates the role
    /// it was asked to provide and any peers it had to seed first.
    pub parent_id: Option<i64>,
    pub child_id: Option<i64>,
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

    let mut cfg = Config::from_env().expect("config");
    // Make sure no on-disk paths leak between tests.
    cfg.database_url = "sqlite::memory:".to_string();
    cfg.static_dir = "./frontend/dist".to_string();

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
    }
}

/// Boot the app with a completed setup: one parent (and one child if
/// `role == Child`), all credential keys present, and a signed session
/// cookie for the requested role pre-installed in the test server's
/// jar.
pub async fn boot_setup_complete(role: AccountType) -> (TestApp, AuthCookie) {
    let mut app = boot().await;

    seed_credentials(&app.pool).await;
    set_config_value(&app.pool, KEY_SETUP_COMPLETE, "true")
        .await
        .expect("setup_complete");

    let parent_id = insert_account(
        &app.pool,
        "google-parent-1",
        "parent@example.test",
        "Parent One",
        AccountType::Parent,
    )
    .await;
    app.parent_id = Some(parent_id);

    let child_id = if matches!(role, AccountType::Child) {
        let id = insert_account(
            &app.pool,
            "google-child-1",
            "child@example.test",
            "Child One",
            AccountType::Child,
        )
        .await;
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

    seed_credentials(&app.pool).await;
    set_config_value(&app.pool, KEY_SETUP_COMPLETE, "true")
        .await
        .expect("setup_complete");

    let parent_id = insert_account(
        &app.pool,
        "google-parent-1",
        "parent@example.test",
        "Parent One",
        AccountType::Parent,
    )
    .await;
    let child_id = insert_account(
        &app.pool,
        "google-child-1",
        "child@example.test",
        "Child One",
        AccountType::Child,
    )
    .await;
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

/// Insert a fully-populated `accounts` row using deterministic test
/// tokens. Returns the new `accounts.id`.
pub async fn insert_account(
    pool: &SqlitePool,
    google_id: &str,
    email: &str,
    display_name: &str,
    account_type: AccountType,
) -> i64 {
    let one_hour_from_now = chrono::Utc::now().timestamp() + 3600;
    let id: i64 = sqlx::query_scalar(
        "INSERT INTO accounts \
            (google_id, email, display_name, avatar_url, account_type, \
             access_token, refresh_token, token_expires_at) \
         VALUES (?, ?, ?, NULL, ?, ?, ?, ?) \
         RETURNING id",
    )
    .bind(google_id)
    .bind(email)
    .bind(display_name)
    .bind(account_type.as_str())
    .bind("test-access-token")
    .bind("test-refresh-token")
    .bind(one_hour_from_now)
    .fetch_one(pool)
    .await
    .expect("insert account");
    id
}

/// Insert a session row for `account_id` and return a signed cookie
/// representing it. The cookie is signed with the same `Key` the app
/// was built with so the server's cookie middleware will verify it.
pub async fn mint_session_cookie(app: &TestApp, account_id: i64) -> AuthCookie {
    use rand::distributions::Alphanumeric;
    use rand::Rng;

    let session_id: String = rand::thread_rng()
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

/// Minimal credential set needed to keep middleware happy.
pub async fn seed_credentials(pool: &SqlitePool) {
    let pairs: &[(&str, &str)] = &[
        (KEY_GOOGLE_CLIENT_ID, "test-client-id"),
        (KEY_GOOGLE_CLIENT_SECRET, "test-client-secret"),
        (
            KEY_GOOGLE_REDIRECT_URI,
            "http://localhost:3000/api/auth/callback",
        ),
        (KEY_YOUTUBE_API_KEY, "test-yt-api-key"),
    ];
    for (k, v) in pairs {
        set_config_value(pool, k, v).await.expect("seed config");
    }
}

/// Seed the proxy HMAC secret so dash signing routines that read it
/// don't have to mutate state inside a test.
pub async fn seed_proxy_secret(pool: &SqlitePool) {
    use base64::Engine;
    let bytes = [7u8; 32];
    let encoded = base64::engine::general_purpose::STANDARD.encode(bytes);
    set_config_value(pool, "proxy_hmac_secret", &encoded)
        .await
        .expect("seed proxy secret");
}

/// Convenience: insert a usage limit row for a child for a single
/// day-of-week (0=Sunday).
pub async fn insert_usage_limit(
    pool: &SqlitePool,
    child_id: i64,
    day_of_week: i64,
    max_hours: f64,
    start: &str,
    end: &str,
) {
    sqlx::query(
        "INSERT INTO usage_limits (child_account_id, day_of_week, max_hours, allowed_start_time, allowed_end_time) \
         VALUES (?, ?, ?, ?, ?)",
    )
    .bind(child_id)
    .bind(day_of_week)
    .bind(max_hours)
    .bind(start)
    .bind(end)
    .execute(pool)
    .await
    .expect("insert usage_limit");
}
