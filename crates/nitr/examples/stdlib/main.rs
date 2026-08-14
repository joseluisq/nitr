//! `nitr.*` standard library example: everything Nitr offers to Lua lives
//! on the single `nitr` namespace table — response helpers, JSON, logging,
//! and the crypto/auth primitives. There are no other globals, so scripts
//! never collide with the Lua standard library.
//!
//! Run from the repository root:
//!
//! ```sh
//! cargo run --example stdlib
//!
//! curl 'http://127.0.0.1:3000/'
//! curl 'http://127.0.0.1:3000/token'
//! curl -X POST 'http://127.0.0.1:3000/password' -d 'hunter2'
//! curl 'http://127.0.0.1:3000/secure'                      # 401
//! curl 'http://127.0.0.1:3000/secure' -H 'authorization: Bearer s3cret'
//! curl 'http://127.0.0.1:3000/whoami' -u 'ada:lovelace'
//! ```

use nitr::{Builtins, Server};

#[tokio::main]
async fn main() -> nitr::Result {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    // `PORT=8080 cargo run --example stdlib` overrides the default port.
    let port = std::env::var("PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(3000);

    Server::builder()
        .listen(([127, 0, 0, 1], port).into())
        .handler_script("crates/nitr/examples/stdlib/app.lua")
        .builtins(Builtins::JSON | Builtins::HTTP | Builtins::LOG | Builtins::CRYPTO)
        .build()
        .await?
        .serve()
        .await
}
