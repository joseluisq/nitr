//! Minimal Nitr embedding: a Lua-scripted backend with a custom Rust global.
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

    Server::builder()
        .listen(([127, 0, 0, 1], 3000).into())
        .handler_script("examples/hello/handler.lua")
        .builtins(Builtins::DEBUG | Builtins::JSON)
        // Expose a custom Rust function to every Lua state.
        .setup(|lua| {
            let greet = lua.create_function(|_, name: String| Ok(format!("Hello, {name}!")))?;
            lua.globals().set("greet", greet)
        })
        .build()
        .await?
        .serve()
        .await
}
