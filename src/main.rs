//! HomeTube — a self-hosted YouTube frontend for kids.
//!
//! Entry point: builds the Axum app, runs migrations, and starts the HTTP server.

use std::net::SocketAddr;

use anyhow::Context;
use tracing::info;

mod config;
mod db;
mod error;
mod middleware;
mod models;
mod routes;
mod services;
mod state;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    init_tracing();

    let cfg = config::Config::from_env().context("loading configuration")?;
    info!(?cfg, "starting hometube");

    // Open the SQLite database (creating it if needed) and run migrations.
    let pool = db::connect(&cfg.database_url).await?;
    db::migrate(&pool).await?;

    let app_state = state::AppState::new(cfg.clone(), pool);
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
