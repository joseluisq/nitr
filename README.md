# Nitr

> A Rust web server embedding [Lua](https://www.lua.org/) for fast, efficient and safe dynamic backends.

**STATUS:** Nitr is in early development and not ready for production use. Feel free to try it out and contribute.

## Overview

Nitr serves HTTP requests with Lua 5.4 scripts. An application is two files: an optional `config.lua` that runs **once** at startup, and a `handler.lua` that runs on **every** request. The server keeps a fixed pool of independent Lua states (one per CPU core by default), so requests execute in parallel without locking, and every script runs under configurable safety limits (restricted stdlib, memory cap, execution timeout).

Nitr is both a **binary** (`nitr`, configured via `nitr.toml`) and a **library crate** (embed the server and register your own Rust functions as Lua globals).

## Features

- Pool of Lua states over a multi-thread runtime: one request per state, no global locks, natural backpressure.
- Safety by default: `io`/`os` excluded from the stdlib (opt-in), 8 MiB memory limit per state, 30 s execution budget enforced by an instruction-count hook (stops `while true do end`) plus an async timeout, `require` confined to the scripts directory, no native Lua modules.
- Built-in Lua APIs: `json` (encode/decode), `fetch` (HTTP client with timeouts), `template` (minijinja), `conn` (SQLite, runs off the async threads), `dbg` (debug logging).
- HTTP correctness: binary-safe request/response bodies, multi-value headers (`Set-Cookie`), parsed query strings, graceful shutdown, no Lua tracebacks leaked to clients (unless dev mode).
- `nitr.toml` configuration with `NITR_*` environment overrides and CLI flags.
- Dev mode (`--dev`): handler hot reload and error details in responses.
- Extensible: register custom Rust functions/modules into every Lua state via the crate API.

## Quick start (binary)

```sh
cargo run
```

With no configuration, Nitr listens on `127.0.0.1:3000` and executes `scripts/handler.lua`. Add a `nitr.toml` to change anything (see [Configuration](#configuration)).

### The handler script

Runs once per request. It receives the config data and the request, and returns the response:

```lua
-- scripts/handler.lua
function(cfg, req)
    return {
        status = 200,
        headers = {
            ["Content-Type"] = "application/json",
            ["Set-Cookie"] = { "a=1", "b=2" },   -- multi-value headers
        },
        body = json:encode({
            message = "Hello, Nitr!",
            path = req.path,
            name = req.query.name,               -- parsed query string
            served_since = cfg.started_at,       -- data from config.lua
        }),
    }
end
```

### The configuration script (optional)

Runs exactly **once** at startup, before requests are served. Use it for setup (e.g. schema migrations); the returned table is passed to the handler on every request. It must return plain data (tables, strings, numbers, booleans) — it is snapshotted and shared with every Lua state.

```lua
-- scripts/config.lua
function(conn)
    conn:execute("CREATE TABLE IF NOT EXISTS users (id INTEGER PRIMARY KEY, name TEXT)")
    return { started_at = os.date("%Y-%m-%dT%H:%M:%S") }
end
```

## Lua API

### Request (`req`)

| Field / method | Description |
| --- | --- |
| `req.method`, `req.path`, `req.remote_addr` | Strings |
| `req.query` | Table of percent-decoded query parameters |
| `req.headers` | Table of request headers |
| `req.uri` | Table: `scheme`, `host`, `port`, `path`, `query`, `authority` |
| `req:text()`, `req:json()`, `req:read()` | Body as string, decoded JSON, or streamed chunks |

### Response (returned table)

`status` (number, default 200), `headers` (value: string, integer, or array of strings), `body` (string; binary-safe).

### Builtins

Enabled via `builtins` in `nitr.toml` (all by default when their settings are present):

| Global | Description |
| --- | --- |
| `json:encode(v)` / `json:decode(s)` | JSON codec (serde) |
| `fetch(method, url, headers?)` → `client:send()` | HTTP client (shared pool, connect/request timeouts, redirect cap). Response: `.status`, `.headers`, `.url`, `:text()`, `:json()`, `:read()` |
| `template:render(name, data?)` | minijinja templates from `templates_dir` |
| `conn:execute/query/query_row/query_one(sql, params?)` | SQLite (`database` file); queries run on a blocking thread pool with a prepared-statement cache |
| `dbg(value)` | Debug-print a Lua value to the log |

## Configuration

`nitr.toml` (see the [annotated example](nitr.toml)), overridable via `NITR_*` env vars and CLI flags (`--config <path>`, `--dev`). Precedence: flags > env > file > defaults.

```toml
listen = "127.0.0.1:3000"
handler_script = "scripts/handler.lua"
config_script = "scripts/config.lua"    # optional
templates_dir = "scripts/templates"     # enables `template`
database = "scripts/file.db"            # enables `conn`
workers = 4                             # Lua states; default: CPU cores
dev_mode = false                        # hot reload + error details
builtins = ["dbg", "fetch", "template", "json", "db"]

[lua]
stdlib = ["math", "table", "string", "utf8", "coroutine", "package"]  # "io"/"os" are opt-in
memory_limit = 8388608                  # bytes, per state
exec_timeout_ms = 30000                 # 0 disables the execution budget
```

## Library usage

```rust
use nitr::{Builtins, Server};

#[tokio::main]
async fn main() -> nitr::Result {
    Server::builder()
        .listen(([127, 0, 0, 1], 3000).into())
        .handler_script("scripts/handler.lua")
        .builtins(Builtins::JSON | Builtins::FETCH)
        // Expose your own Rust functions to every Lua state:
        .setup(|lua| {
            let greet = lua.create_function(|_, name: String| Ok(format!("Hello, {name}!")))?;
            lua.globals().set("greet", greet)
        })
        .build()
        .await?
        .serve() // ctrl-c shuts down gracefully; see serve_with_shutdown()
        .await
}
```

See [examples/hello](examples/hello) (`cargo run --example hello`). For lower-level embedding, `nitr::Runtime` exposes the Lua state, script loading, and the budgeted `call_handler` directly. Errors are a typed `nitr::Error` enum.

## Documentation

Design documents live in [docs/](docs/): [architecture](docs/architecture.md), [crate API](docs/crate-api.md), [configuration](docs/configuration.md), [security model](docs/security.md), [performance](docs/performance.md), and the [roadmap](docs/roadmap.md).

## Name origins

*Niter* or *nitre* is the mineral form of potassium nitrate, KNO3. It is a soft, white, highly soluble mineral found primarily in arid climates or cave deposits.
> https://en.wikipedia.org/wiki/Niter

## Contributions

Unless you explicitly state otherwise, any contribution you intentionally submitted for inclusion in current work, as defined in the Apache-2.0 license, shall be dual licensed as described below, without any additional terms or conditions.

Feel free to submit a [pull request](https://github.com/joseluisq/nitr/pulls) or file an [issue](https://github.com/joseluisq/nitr/issues).

## License

This work is primarily distributed under the terms of both the [MIT license](LICENSE-MIT) and the [Apache License (Version 2.0)](LICENSE-APACHE).

© 2024-present [Jose Quintana](https://joseluisq.net)
