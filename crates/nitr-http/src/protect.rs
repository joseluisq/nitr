//! Rust-side protection enforced before a request reaches Lua: rate
//! limiting and request-size limits. These are infrastructure concerns —
//! implementing them in Lua would let the thing being protected against
//! consume the resources first.

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use hyper::StatusCode;
use hyper::header::HeaderValue;

use crate::config::Config;
use crate::handler::{HttpResponse, plain_response};
use crate::request::LuaRequest;
use nitr_core::Result;

/// Above this many tracked client buckets, stale entries are purged on the
/// next check (a backstop against unbounded growth from IP churn).
const BUCKET_PURGE_THRESHOLD: usize = 10_000;

/// Per-server protection state, shared by all connections.
#[derive(Debug)]
pub(crate) struct Protection {
    max_body_bytes: u64,
    max_uri_bytes: usize,
    trust_request_id: bool,
    dev_mode: bool,
    /// How long a request may wait for a Lua state before it is shed.
    pool_wait: Duration,
    rate: Option<RateLimiter>,
    /// Body-parsing bounds handed to each request.
    form: crate::request::FormLimits,
    /// The compiled `[cors]` policy; `None` when CORS is not configured.
    cors: Option<crate::cors::Cors>,
    /// The compiled `[compression]` policy.
    compression: crate::compress::Compression,
}

impl Protection {
    pub(crate) fn new(cfg: &Config) -> Self {
        Self {
            max_body_bytes: cfg.limits.max_body_bytes,
            max_uri_bytes: cfg.limits.max_uri_bytes,
            trust_request_id: cfg.trust_request_id,
            dev_mode: cfg.dev_mode,
            pool_wait: Duration::from_millis(cfg.limits.pool_wait_ms),
            rate: cfg.rate_limit.enabled.then(|| RateLimiter {
                max: cfg.rate_limit.requests.max(1),
                window: Duration::from_secs(cfg.rate_limit.window.max(1)),
                trust_forwarded_for: cfg.rate_limit.trust_forwarded_for,
                buckets: Mutex::new(HashMap::new()),
            }),
            form: crate::request::FormLimits {
                max_parts: cfg.limits.max_form_parts.max(1),
                max_field_bytes: cfg.limits.max_field_bytes,
                max_file_bytes: cfg.limits.max_file_bytes,
            },
            cors: crate::cors::Cors::new(&cfg.cors),
            compression: crate::compress::Compression::new(&cfg.compression),
        }
    }

    /// The compiled CORS policy, or `None` when CORS is not configured.
    pub(crate) fn cors(&self) -> Option<&crate::cors::Cors> {
        self.cors.as_ref()
    }

    /// The compiled compression policy.
    pub(crate) fn compression(&self) -> &crate::compress::Compression {
        &self.compression
    }

    /// Body-parsing bounds handed to each request.
    pub(crate) fn form_limits(&self) -> crate::request::FormLimits {
        self.form
    }

    /// Whether error responses may carry internal detail.
    pub(crate) fn dev_mode(&self) -> bool {
        self.dev_mode
    }

    /// The checkout wait budget; zero means wait indefinitely.
    pub(crate) fn pool_wait(&self) -> Duration {
        self.pool_wait
    }

    /// The configured request body ceiling, enforced as the body is read.
    pub(crate) fn max_body_bytes(&self) -> u64 {
        self.max_body_bytes
    }

    /// The id for a request: a trusted, well-formed inbound `X-Request-ID`
    /// when configured, otherwise a fresh UUIDv7 (time-sortable).
    pub(crate) fn request_id(&self, req: &hyper::Request<hyper::body::Incoming>) -> String {
        self.request_id_for_parts(req.headers())
    }

    /// [`request_id`](Self::request_id) over bare headers (used by the
    /// in-process test client, whose body type differs).
    pub(crate) fn request_id_for_parts(&self, headers: &hyper::HeaderMap) -> String {
        if self.trust_request_id
            && let Some(id) = headers
                .get("x-request-id")
                .and_then(|v| v.to_str().ok())
                .filter(|v| {
                    !v.is_empty() && v.len() <= 64 && v.bytes().all(|b| b.is_ascii_graphic())
                })
        {
            return id.to_string();
        }
        uuid::Uuid::now_v7().to_string()
    }

    /// Runs the pre-Lua checks; `Some` is the rejection response.
    pub(crate) fn check(&self, req: &LuaRequest) -> Option<Result<HttpResponse>> {
        if let Some(rate) = &self.rate
            && let Err(retry_after) = rate.check(req)
        {
            tracing::debug!(peer = %req.peer_addr, "request rate limited");
            return Some(
                plain_response(StatusCode::TOO_MANY_REQUESTS, "Too Many Requests").map(
                    |mut resp| {
                        if let Ok(value) = HeaderValue::from_str(&retry_after.to_string()) {
                            resp.headers_mut().insert(hyper::header::RETRY_AFTER, value);
                        }
                        resp
                    },
                ),
            );
        }

        if uri_len(req) > self.max_uri_bytes {
            return Some(plain_response(StatusCode::URI_TOO_LONG, "URI Too Long"));
        }

        // Declared body size; a chunked body that lies is caught later by
        // the state's memory limit when the handler reads it.
        let declared = req
            .req
            .headers()
            .get(hyper::header::CONTENT_LENGTH)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.parse::<u64>().ok());
        if declared.is_some_and(|len| len > self.max_body_bytes) {
            return Some(plain_response(
                StatusCode::PAYLOAD_TOO_LARGE,
                "Payload Too Large",
            ));
        }

        None
    }
}

fn uri_len(req: &LuaRequest) -> usize {
    let uri = req.req.uri();
    uri.path().len() + uri.query().map_or(0, |q| q.len() + 1)
}

/// A fixed-window request counter per client IP.
#[derive(Debug)]
struct RateLimiter {
    max: u32,
    window: Duration,
    trust_forwarded_for: bool,
    buckets: Mutex<HashMap<IpAddr, (Instant, u32)>>,
}

impl RateLimiter {
    /// Returns `Err(retry_after_seconds)` when the client exceeded its
    /// budget for the current window.
    fn check(&self, req: &LuaRequest) -> std::result::Result<(), u64> {
        let ip = self.client_ip(req);
        let now = Instant::now();
        let mut buckets = match self.buckets.lock() {
            Ok(guard) => guard,
            // Poisoning is unreachable in practice (no panics while held);
            // failing open beats taking the server down.
            Err(_) => return Ok(()),
        };
        if buckets.len() > BUCKET_PURGE_THRESHOLD {
            let window = self.window;
            buckets.retain(|_, (start, _)| now.duration_since(*start) < window);
        }
        let bucket = buckets.entry(ip).or_insert((now, 0));
        if now.duration_since(bucket.0) >= self.window {
            *bucket = (now, 0);
        }
        bucket.1 += 1;
        if bucket.1 > self.max {
            let elapsed = now.duration_since(bucket.0);
            let retry = self.window.saturating_sub(elapsed).as_secs().max(1);
            return Err(retry);
        }
        Ok(())
    }

    /// The IP the budget is keyed by: the first `X-Forwarded-For` entry
    /// when explicitly trusted (behind a proxy), else the peer address.
    fn client_ip(&self, req: &LuaRequest) -> IpAddr {
        if self.trust_forwarded_for
            && let Some(ip) = req
                .req
                .headers()
                .get("x-forwarded-for")
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.split(',').next())
                .and_then(|v| v.trim().parse().ok())
        {
            return ip;
        }
        req.peer_addr.ip()
    }
}
