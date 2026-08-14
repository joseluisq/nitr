//! Minimal Nitr embedding: a Lua-scripted backend with a custom Rust
//! extension module mounted at `nitr.hello`.
//!
//! Run from the repository root:
//!
//! ```sh
//! cargo run --example hello
//! curl 'http://127.0.0.1:3000/?name=Nitr'
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

    // `PORT=8080 cargo run --example hello` overrides the default port.
    let port = std::env::var("PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(3000);

    Server::builder()
        .listen(([127, 0, 0, 1], port).into())
        .handler_script("crates/nitr/examples/hello/handler.lua")
        .builtins(Builtins::DEBUG | Builtins::JSON)
        // A Rust extension module: the returned table is mounted at
        // `nitr.hello` in every pooled Lua state, next to the builtins.
        .module("hello", |lua| {
            let t = lua.create_table()?;
            t.set(
                "greet",
                lua.create_function(|_, name: String| Ok(format!("Hello, {name}!")))?,
            )?;
            Ok(t)
        })
        .build()
        .await?
        .serve()
        .await
}
