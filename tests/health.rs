//! Liveness probe coverage.
//!
//! `GET /api/health` is the endpoint the Docker `HEALTHCHECK` directive
//! hits on every interval, so it must:
//!
//! - return `200 ok` while the database is reachable, and
//! - degrade to `503` when the underlying SQLite pool is closed.
//!
//! The 503 case is deterministic (no race conditions): we close the
//! pool from the test, then issue the request, then assert.

mod common;

use axum::http::StatusCode;
use common::boot;

#[tokio::test]
async fn health_returns_ok_after_boot() {
    let app = boot().await;
    let res = app.server.get("/api/health").await;
    assert_eq!(res.status_code(), StatusCode::OK);
    assert_eq!(res.text(), "ok");
}

#[tokio::test]
async fn health_returns_503_when_pool_is_closed() {
    let app = boot().await;
    // Closing the pool short-circuits any subsequent SELECT; the
    // health handler maps that to 503.
    app.pool.close().await;
    let res = app.server.get("/api/health").await;
    assert_eq!(res.status_code(), StatusCode::SERVICE_UNAVAILABLE);
}
