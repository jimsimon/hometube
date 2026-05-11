//! HomeTube — a self-hosted YouTube frontend for kids.
//!
//! Entry point: builds the Axum app, runs migrations, and starts the HTTP server.

use std::net::SocketAddr;

use anyhow::Context;
use base64::Engine;
use rand::RngCore;
use sqlx::SqlitePool;
use tower_cookies::Key;
use tracing::info;

mod config;
mod db;
mod error;
mod middleware;
mod models;
mod routes;
mod services;
mod state;

use crate::services::setup::{get_config_value, set_config_value, KEY_COOKIE_SECRET};

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

    let app_state = state::AppState::new(cfg.clone(), pool, cookie_key);
    let app = routes::router(app_state);

    let addr: SocketAddr = format!("{}:{}", cfg.host, cfg.port).parse()?;
    info!(%addr, "listening");

    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, app).await?;

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
    rand::thread_rng().fill_bytes(&mut bytes);
    let encoded = engine.encode(bytes);
    set_config_value(pool, KEY_COOKIE_SECRET, &encoded).await?;
    info!("generated new cookie signing key");
    Ok(Key::from(&bytes[..]))
}
