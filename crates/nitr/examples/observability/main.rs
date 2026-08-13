//! Observability + protection: structured logging from Lua (`log.*`),
//! request ids on every response, per-client rate limiting, and
//! request-size limits — all enforced in Rust before Lua runs.
//!
//! Run from the repository root (RUST_LOG shows the Lua log events with
//! their request span):
//!
//! ```sh
//! RUST_LOG=info,lua=debug cargo run --example observability
//!
//! curl -i 'http://127.0.0.1:3000/'            # note the X-Request-ID header
//! for i in $(seq 1 6); do curl -s -o /dev/null -w '%{http_code}\n' \
//!     'http://127.0.0.1:3000/'; done          # 5 pass, then 429
//! curl -i "http://127.0.0.1:3000/?q=$(python3 -c 'print("x"*9000)')"  # 414
//! curl -i -X POST 'http://127.0.0.1:3000/echo' --data-binary @/dev/zero \
//!     -H 'Content-Length: 2000000'            # 413
//! ```

use nitr::{Builtins, Config, Server};

#[tokio::main]
async fn main() -> nitr::Result {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info,lua=debug")),
        )
        .init();

    // `PORT=8080 cargo run --example observability` overrides the port.
    let port = std::env::var("PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(3000);

    // Protection is configuration, not code: the same settings work from
    // nitr.toml ([rate_limit] / [limits] sections).
    let mut cfg = Config::default();
    cfg.rate_limit.enabled = true;
    cfg.rate_limit.requests = 5;
    cfg.rate_limit.window = 10;
    cfg.limits.max_body_bytes = 1024 * 1024; // 1 MiB

    Server::builder()
        .config(cfg)
        .listen(([127, 0, 0, 1], port).into())
        .handler_script("crates/nitr/examples/observability/app.lua")
        .builtins(Builtins::JSON | Builtins::HTTP | Builtins::LOG)
        .build()
        .await?
        .serve()
        .await
}
