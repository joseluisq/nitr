//! Streaming response bodies: a writer-callback CSV download and a
//! coroutine-iterator body. Chunks reach the client as they are produced,
//! with backpressure when the client reads slowly.
//!
//! Run from the repository root:
//!
//! ```sh
//! cargo run --example streaming
//!
//! curl 'http://127.0.0.1:3000/report.csv'   # writer callback
//! curl 'http://127.0.0.1:3000/chunks'       # coroutine iterator
//! curl --limit-rate 1K 'http://127.0.0.1:3000/report.csv'  # backpressure
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

    // `PORT=8080 cargo run --example streaming` overrides the default port.
    let port = std::env::var("PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(3000);

    Server::builder()
        .listen(([127, 0, 0, 1], port).into())
        .handler_script("crates/nitr/examples/streaming/app.lua")
        .builtins(Builtins::JSON | Builtins::HTTP)
        .build()
        .await?
        .serve()
        .await
}
