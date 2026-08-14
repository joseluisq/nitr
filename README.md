# Nitr

> A Rust web server embedding [Lua](https://www.lua.org/) for fast, efficient and safe dynamic backends.

**STATUS:** Nitr is in early development and not ready for production use. Feel free to try it out and contribute.

## Overview

Nitr serves HTTP requests with Lua 5.4 scripts. An application is two files: an optional `config.lua` that runs **once** at startup, and an `app.lua` that builds the application (routes and middleware) once per Lua state. The server keeps a fixed pool of independent Lua states (one per CPU core by default), so requests execute in parallel without locking, and every script runs under configurable safety limits (restricted stdlib, memory cap, execution timeout).

Everything Nitr exposes to Lua lives on one global namespace table, `nitr` — `nitr.app()`, `nitr.json`, `nitr.db`, `nitr.crypto`, and so on. Nitr registers no other globals, so scripts never collide with the Lua standard library, and your own Rust extensions mount on the very same namespace.

Nitr is both a **binary** (`nitr`, configured via `nitr.toml`) and a **library crate** (embed the server and register your own Rust modules as `nitr.*` APIs).

## Features

- **Pool of Lua states over a multi-thread runtime:** one request per state, no global locks, natural backpressure.
- **Safety by default**: `io`/`os` excluded from the stdlib (opt-in), 8 MiB memory limit per state, 30 s execution budget enforced by an instruction-count hook (stops `while true do end`) plus an async timeout, `require` confined to the scripts directory, no native Lua modules.
- **One namespaced standard library:** `nitr.json`, `nitr.fetch` (HTTP client with SSRF policy), `nitr.template` (minijinja), `nitr.db` (SQLite, runs off the async threads), `nitr.log`, `nitr.crypto`/`nitr.auth`, `nitr.dbg`.
- **Rust-side routing (`nitr.app()`):** path parameters, middleware chains composed once at load, per-app error handler, 404/405 answered without entering Lua.
- **HTTP correctness:** binary-safe request/response bodies, multi-value headers (`Set-Cookie`), parsed query strings, graceful shutdown, no Lua tracebacks leaked to clients (unless dev mode).
- **Easy configuration:** `nitr.toml` configuration with `NITR_*` environment overrides and CLI flags.
- **Dev mode (`--dev`)**: handler hot reload and error details in responses.
- **Extensible:** `ServerBuilder::module("name", ...)` mounts a Rust table at `nitr.name` in every Lua state, third-party extension crates need no fork.

## Quick start (binary)

```sh
cargo run
```

With no configuration, Nitr listens on `127.0.0.1:3000` and executes `scripts/handler.lua`. Add a `nitr.toml` to change anything (see [Configuration](#configuration)). `nitr init` scaffolds a complete application; `nitr check` validates it and `nitr test` runs its Lua tests in-process.

### The handler script

Returns the application built with `nitr.app()`. The script runs once per Lua state; routes and middleware are compiled at load time, and only matching requests reach Lua:

```lua
-- scripts/handler.lua
local app = nitr.app()

-- Middleware wraps the next handler; composed once, not per request.
app:use(function(next)
    return function(req)
        nitr.log.info("request", { path = req.path })
        return next(req)
    end
end)

app:get("/users/:id", function(req)
    return nitr.json({
        message = "Hello, Nitr!",
        id = req.params.id,                  -- path parameter
        name = req.query.name,               -- parsed query string
        served_since = nitr.cfg.started_at,  -- data from config.lua
    })
end)

app:on_error(function(err, req)
    return nitr.error(500, { code = "INTERNAL" })
end)

return app
```

### The configuration script (optional)

Runs exactly **once** at startup, before requests are served. Use it for setup (e.g. schema migrations); the returned table is available to handlers as `nitr.cfg`. It must return plain data (tables, strings, numbers, booleans) — it is snapshotted and shared with every Lua state. The database connection is passed in as its argument.

```lua
-- scripts/config.lua
function(db)
    db:execute("CREATE TABLE IF NOT EXISTS users (id INTEGER PRIMARY KEY, name TEXT)")
    return { started_at = os.date("%Y-%m-%dT%H:%M:%S") }
end
```

## Lua API

Every Nitr API is a field of the global `nitr` table; nothing else is registered.

### Application (`nitr.app()`)

| Method | Description |
| --- | --- |
| `app:get/post/put/delete/patch/head/options(path, ...fns)` | Register a route; `:name` captures a parameter, a trailing `*` captures the rest. All but the last function are route middleware |
| `app:use(fn)` | Global middleware, `function(next) return function(req) ... end end`; must precede routes |
| `app:on_error(fn)` | `function(err, req)` — the app-wide error response |
| `app:static(mount, dir, opts?)` | Serve files from Rust (`{ spa = true, cache_control = "..." }`) |
| `nitr.cfg` | The configuration script's snapshot |

### Request (`req`)

| Field / method | Description |
| --- | --- |
| `req.method`, `req.path`, `req.remote_addr` | Strings |
| `req.query` | Table of percent-decoded query parameters |
| `req.headers` | Table of request headers |
| `req.uri` | Table: `scheme`, `host`, `port`, `path`, `query`, `authority` |
| `req.params` | Table of path parameters |
| `req.id` | Request id (UUIDv7, echoed as `X-Request-ID`) |
| `req.cookies` | `req.cookies.name`, `req.cookies:verify(name, secret)` |
| `req:text()`, `req:json()`, `req:read()`, `req:accepts(...)` | Body as string, decoded JSON, streamed chunks; content negotiation |

### Response (returned table)

`status` (number, default 200), `headers` (value: string, integer, or array of strings), `body` (string; binary-safe, or a function for a streaming body). The helpers below build these tables for you.

| Helper | Description |
| --- | --- |
| `nitr.json(v, status?)` | JSON response; `resp.cookies:set(...)` / `:set_signed(...)` attach cookies |
| `nitr.text(s, status?)`, `nitr.html(s, status?)` | Plain-text / HTML responses |
| `nitr.redirect(location, status?)`, `nitr.status(code)` | Redirects and bare status responses |
| `nitr.error(code, body?)` | Error response; a table body is rendered as JSON |
| `nitr.negotiate(req, offers)` | Content negotiation over the `Accept` header (406 when nothing matches) |
| `nitr.sse(fn)` | Server-Sent Events stream; `fn(send)` calls `send(event, data)` |

### Standard library

The `nitr.*` standard library provides building blocks — enable the features you need via `[std] features` in `nitr.toml` (default: the minimal `json`, `http`, `log` set), or replace them with your own modules:

| Module | Description |
| --- | --- |
| `nitr.json:encode(v)` / `nitr.json:decode(s)` | JSON codec (serde); callable as the response helper above |
| `nitr.fetch(method, url, opts?)` → `client:send()` | HTTP client (shared pool, timeouts, SSRF policy, per-hop redirect checks). Response: `.status`, `.headers`, `.url`, `:text()`, `:json()`, `:read()` |
| `nitr.await_all({...})` | Run several `fetch` handles concurrently, capped by `fetch.max_concurrent` |
| `nitr.template:render(name, data?)` | minijinja templates from `templates_dir` |
| `nitr.db:execute/query/query_row/query_one(sql, params?)` | SQLite (`database` file); queries run on a blocking thread pool with a prepared-statement cache |
| `nitr.db:transaction(fn)` | Atomic transaction (nestable via savepoints); rolls back on error |
| `nitr.log.debug/info/warn/error(msg, fields?)` | Structured logging into the request span |
| `nitr.crypto.*` | `sha256`, `hmac_sha256`, `random_bytes`, `constant_time_eq`, `password_hash`/`password_verify` (argon2id) |
| `nitr.auth.basic(req)` / `nitr.auth.bearer(req)` | Parse `Authorization` credentials |
| `nitr.dbg(value)` | Debug-print a Lua value to the log |

## Configuration

`nitr.toml` (see the [annotated example](nitr.toml)), overridable via `NITR_*` env vars and CLI flags (`--config <path>`, `--dev`). Precedence: flags > env > file > defaults.

```toml
listen = "127.0.0.1:3000"
handler_script = "scripts/handler.lua"
config_script = "scripts/config.lua"    # optional
templates_dir = "scripts/templates"     # enables `nitr.template`
database = "scripts/file.db"            # enables `nitr.db`
workers = 4                             # Lua states; default: CPU cores
dev_mode = false                        # hot reload + error details

[std]
# `nitr.*` standard library features; default: ["json", "http", "log"]
features = ["dbg", "fetch", "template", "json", "db", "http", "log", "crypto"]

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
        // Expose your own Rust code as `nitr.greet` in every Lua state:
        .module("greet", |lua| {
            let t = lua.create_table()?;
            t.set("hello", lua.create_function(|_, name: String| {
                Ok(format!("Hello, {name}!"))
            })?)?;
            Ok(t)
        })
        .build()
        .await?
        .serve() // ctrl-c shuts down gracefully; see serve_with_shutdown()
        .await
}
```

Modules are the extension boundary: the closure runs once per pooled state (and on every reload), and a name that collides with a builtin or another module fails at build time. See [examples/extension](crates/nitr/examples/extension) for a stateful module shared across states, and [examples/stdlib](crates/nitr/examples/stdlib) for a tour of `nitr.*`.

For lower-level embedding, `nitr::Runtime` exposes the Lua state, `register_module()`, script loading, and the budgeted `call_function()` directly — no HTTP involved. Errors are a typed `nitr::Error` enum.

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
