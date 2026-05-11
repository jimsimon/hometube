//! Per-account / per-IP token bucket for proxy endpoints.
//!
//! The proxy endpoints (`/api/proxy/segment`, `/api/proxy/audio`,
//! `/api/proxy/thumbnail/:videoId`) are the only routes a child UI hits
//! at high frequency, and they're the ones that fan out to the network
//! / disk. To keep a stuck or buggy client from exhausting the server's
//! egress, every request is gated by a simple in-process token bucket.
//!
//! Buckets are keyed by `account_id` when the request carries a session,
//! and by `peer IP` otherwise. Each bucket carries `BUCKET_CAPACITY`
//! tokens that refill at `BUCKET_REFILL_PER_SEC` tokens / second; over
//! the long term that's ~`BUCKET_CAPACITY * 60 / window` ≈ 200 req/min
//! sustained.
//!
//! On a denied request we respond `429 Too Many Requests` with a
//! `Retry-After` header set to the number of whole seconds until the
//! bucket has at least one token again.

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::OnceLock;
use std::time::Instant;

use axum::extract::ConnectInfo;
use axum::http::{header, HeaderValue, Request, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use tokio::sync::Mutex;

use crate::middleware::auth::CurrentAccount;

/// Maximum tokens the bucket can hold.
const BUCKET_CAPACITY: f64 = 200.0;
/// Tokens added per second when below capacity.
const BUCKET_REFILL_PER_SEC: f64 = 200.0 / 60.0;
/// Tokens consumed by a single request.
const COST_PER_REQUEST: f64 = 1.0;

#[derive(Debug, Hash, Eq, PartialEq, Clone)]
enum BucketKey {
    Account(i64),
    Ip(IpAddr),
}

#[derive(Debug, Clone, Copy)]
struct Bucket {
    tokens: f64,
    last_refill: Instant,
}

impl Bucket {
    fn new() -> Self {
        Self {
            tokens: BUCKET_CAPACITY,
            last_refill: Instant::now(),
        }
    }

    fn refill(&mut self, now: Instant) {
        let elapsed = now.duration_since(self.last_refill).as_secs_f64();
        if elapsed > 0.0 {
            self.tokens = (self.tokens + elapsed * BUCKET_REFILL_PER_SEC).min(BUCKET_CAPACITY);
            self.last_refill = now;
        }
    }

    /// Try to consume `cost` tokens. Returns the number of seconds until
    /// the bucket would be ready if the call failed.
    fn try_acquire(&mut self, cost: f64) -> Result<(), u64> {
        let now = Instant::now();
        self.refill(now);
        if self.tokens >= cost {
            self.tokens -= cost;
            Ok(())
        } else {
            let deficit = cost - self.tokens;
            let secs = (deficit / BUCKET_REFILL_PER_SEC).ceil() as u64;
            Err(secs.max(1))
        }
    }
}

fn buckets() -> &'static Mutex<HashMap<BucketKey, Bucket>> {
    static BUCKETS: OnceLock<Mutex<HashMap<BucketKey, Bucket>>> = OnceLock::new();
    BUCKETS.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Axum middleware: gate the inner service on a per-account / per-IP
/// token bucket. Apply *after* the auth layer so authenticated callers
/// share a quota with their session rather than their IP.
pub async fn rate_limit_proxies(req: Request<axum::body::Body>, next: Next) -> Response {
    let key = derive_key(&req);
    let mut map = buckets().lock().await;
    let bucket = map.entry(key.clone()).or_insert_with(Bucket::new);
    match bucket.try_acquire(COST_PER_REQUEST) {
        Ok(()) => {
            drop(map);
            next.run(req).await
        }
        Err(retry_after_secs) => {
            drop(map);
            let mut response =
                (StatusCode::TOO_MANY_REQUESTS, "rate limit exceeded\n").into_response();
            if let Ok(value) = HeaderValue::from_str(&retry_after_secs.to_string()) {
                response.headers_mut().insert(header::RETRY_AFTER, value);
            }
            response
        }
    }
}

fn derive_key(req: &Request<axum::body::Body>) -> BucketKey {
    if let Some(current) = req.extensions().get::<CurrentAccount>() {
        return BucketKey::Account(current.id);
    }
    if let Some(connect_info) = req.extensions().get::<ConnectInfo<std::net::SocketAddr>>() {
        return BucketKey::Ip(connect_info.0.ip());
    }
    // Fall back to a sentinel value if no peer info is available — the
    // bucket effectively becomes a global one in that case.
    BucketKey::Ip(IpAddr::from([0, 0, 0, 0]))
}
