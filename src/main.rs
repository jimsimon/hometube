//! HomeTube — a self-hosted YouTube frontend for kids.
//!
//! Entry point: builds the Axum app, runs migrations, seeds default
//! cron jobs, starts the in-process scheduler, and starts the HTTP
//! server.

use std::net::SocketAddr;

use anyhow::Context;
use base64::Engine;
use rand::Rng;
use sqlx::SqlitePool;
use tower_cookies::Key;
use tracing::{info, warn};

use hometube::{config, db, state};

use hometube::services::cron::{seed_default_jobs, seed_ytdlp_info, Scheduler};
use hometube::services::dash;
use hometube::services::feed_refresher;
use hometube::services::setup::{
    get_config_value, set_config_value, KEY_COOKIE_SECRET, KEY_YTDLP_COOKIES,
};
use hometube::services::video_cache::{DEFAULT_TTL_HOURS, KEY_METADATA_CACHE_TTL_HOURS};
use hometube::services::ytdlp;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    init_tracing();

    let cfg = config::Config::from_env().context("loading configuration")?;
    info!(?cfg, "starting hometube");

    // Open the SQLite database (creating it if needed) and run migrations.
    let pool = db::connect(&cfg.database_url).await?;
    db::migrate(&pool).await?;

    // Cookie signing key: stored as a base64 blob in `app_config` so it
    // survives restarts. Generated on first run.
    let cookie_key = ensure_cookie_key(&pool).await?;

    // Phase 5 startup helpers: ensure the proxy HMAC secret exists and
    // seed the metadata-cache TTL with its default value if unset.
    dash::ensure_proxy_secret(&pool).await?;
    ensure_default_metadata_ttl(&pool).await?;

    // Phase 12: seed cron-job defaults + the singleton ytdlp_info row.
    if let Err(err) = seed_default_jobs(&pool).await {
        warn!(%err, "failed to seed default cron jobs");
    }
    if let Err(err) = seed_ytdlp_info(&pool, &cfg).await {
        warn!(%err, "failed to seed ytdlp_info");
    }

    // Sync yt-dlp cookies file from DB to disk on startup so the file
    // is present after container restarts.
    match get_config_value(&pool, KEY_YTDLP_COOKIES).await {
        Ok(Some(ref cookies)) if !cookies.trim().is_empty() => {
            if let Err(e) = ytdlp::sync_cookies_to_disk(Some(cookies)) {
                warn!(error = %e, "failed to sync cookies file to disk on startup");
            }
        }
        Ok(_) => {}
        Err(e) => {
            warn!(error = %e, "failed to read ytdlp_cookies from app_config");
        }
    }

    // Build the cron scheduler. Failures here are logged + skipped —
    // the app must still boot (and serve the parent UI) even if the
    // scheduler can't start.
    let scheduler = match Scheduler::new(pool.clone(), cfg.clone()).await {
        Ok(sched) => {
            if let Err(err) = sched.register_all().await {
                warn!(%err, "registering cron jobs");
            }
            if let Err(err) = sched.start().await {
                warn!(%err, "starting cron scheduler");
            }
            Some(sched)
        }
        Err(err) => {
            warn!(%err, "could not create cron scheduler; jobs will not run");
            None
        }
    };

    // Backfill the feed_sources cache with every currently-allowlisted
    // channel. New rows get `next_poll_at = 0` so the refresher
    // schedules them on its next tick. Idempotent — re-runs every
    // startup are no-ops.
    if let Err(err) = backfill_feed_sources(&pool).await {
        warn!(%err, "failed to backfill feed_sources from allowlist");
    }

    // Spawn the new-videos feed refresher. Runs forever; loops on
    // `feed_sources.next_poll_at` and writes results into the
    // `feed_source_items` cache that backs `/api/feed/new-videos`.
    feed_refresher::spawn(pool.clone());

    let mut app_state = state::AppState::new(cfg.clone(), pool, cookie_key);
    if let Some(sched) = scheduler {
        app_state = app_state.with_scheduler(sched);
    }
    let app = hometube::routes::router(app_state);

    let addr: SocketAddr = format!("{}:{}", cfg.host, cfg.port).parse()?;
    info!(%addr, "listening");

    let listener = tokio::net::TcpListener::bind(addr).await?;
    // Use ConnectInfo so the rate-limit middleware can fall back to
    // the peer IP when a request is unauthenticated.
    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .await?;

    Ok(())
}

fn init_tracing() {
    use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt, EnvFilter};

    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("info,hometube=debug,tower_http=info"));

    tracing_subscriber::registry()
        .with(filter)
        .with(tracing_subscriber::fmt::layer())
        .init();
}

/// Load the cookie signing key from `app_config`, generating + persisting
/// a fresh 64-byte secret on first run. The base64 alphabet is the
/// URL-safe variant without padding so the value is safe to copy/paste
/// out of SQLite if needed.
async fn ensure_cookie_key(pool: &SqlitePool) -> anyhow::Result<Key> {
    let engine = base64::engine::general_purpose::URL_SAFE_NO_PAD;

    if let Some(stored) = get_config_value(pool, KEY_COOKIE_SECRET).await? {
        let bytes = engine
            .decode(stored.as_bytes())
            .context("decoding cookie_secret from app_config")?;
        if bytes.len() >= 64 {
            return Ok(Key::from(bytes.as_slice()));
        }
        info!("existing cookie_secret too short; regenerating");
    }

    // tower-cookies's `Key` requires at least 64 bytes of input.
    let mut bytes = [0u8; 64];
    rand::rng().fill_bytes(&mut bytes);
    let encoded = engine.encode(bytes);
    set_config_value(pool, KEY_COOKIE_SECRET, &encoded).await?;
    info!("generated new cookie signing key");
    Ok(Key::from(&bytes[..]))
}

/// Insert a `feed_sources` row for every distinct
/// `allowlisted_channels.channel_id` so the background refresher has
/// something to poll on a fresh database / after the schema migration.
async fn backfill_feed_sources(pool: &sqlx::SqlitePool) -> anyhow::Result<()> {
    // The trailing `WHERE true` disambiguates the UPSERT's `ON CONFLICT`
    // from a potential JOIN `ON` clause in the SELECT. Without it SQLite
    // reports `near "DO": syntax error`. See https://www.sqlite.org/lang_upsert.html
    // ("Parsing Ambiguity").
    sqlx::query(
        "INSERT INTO feed_sources (kind, source_id, next_poll_at) \
         SELECT 'channel', channel_id, 0 FROM allowlisted_channels WHERE true \
         ON CONFLICT(kind, source_id) DO NOTHING",
    )
    .execute(pool)
    .await?;
    Ok(())
}

/// Seed `metadata_cache_ttl_hours` with the default if the parent
/// hasn't customised it yet. The cache reads the live value on every
/// lookup so this is just a UX convenience for the parent settings UI.
async fn ensure_default_metadata_ttl(pool: &sqlx::SqlitePool) -> anyhow::Result<()> {
    if get_config_value(pool, KEY_METADATA_CACHE_TTL_HOURS)
        .await?
        .is_none()
    {
        set_config_value(
            pool,
            KEY_METADATA_CACHE_TTL_HOURS,
            &DEFAULT_TTL_HOURS.to_string(),
        )
        .await?;
    }
    Ok(())
}
