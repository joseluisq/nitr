# Error handling

How failures behave in a Nitr application, and what you can rely on.
Written for application authors; the internals live in the crate docs.

Nitr's error model has exactly three layers, from most to least common:

| Failure | What happens |
| --- | --- |
| A Lua error, timeout, or memory limit in your handler | Classified into a structured error, offered to your `on_error` handler, otherwise answered with a `500` |
| Invalid input or an overloaded server | Rejected in Rust before your code runs (`404`, `405`, `413`, `414`, `429`, `503`, …) |
| A panic in Rust code (a bug in Nitr or an extension module) | Contained at the request boundary: the response is a `500`, the Lua state is recycled, the process and every other connection survive |

`Result`-style errors are the normal currency; panic containment is a
last-resort safety net for genuine bugs, not something an application can
or should trigger deliberately. If you ever see `kind = "panic"`, it is
worth [reporting](#reporting-a-bug).

## The error value

An error handler (and the structured log line) sees the failure as a
table with a closed set of fields:

```lua
{
  kind = "lua",            -- always present, see below
  message = "attempt to index a nil value (local 'user')",
  source = "app.lua",      -- the failing chunk, when known
  line = 42,               -- the failing line, when known
  module = "nitr.db",      -- the failing module, when attributed
  traceback = "...",       -- bounded Lua call stack (innermost first)
  cause = { "...", ... },  -- bounded underlying Rust error chain
}
```

`kind` is one of a closed set — Lua code cannot forge a kind, so
branching on it is stable in a way matching on message text never is:

| `err.kind` | Meaning |
| --- | --- |
| `"lua"` | An error raised by your script (or a Lua library it calls) |
| `"nitr"` | A `nitr.*` builtin failed, or was called wrongly (bad argument, invalid response shape) |
| `"module"` | A registered extension module (`nitr.ext.*`) failed |
| `"timeout"` | The handler exceeded its execution budget (`[lua] exec_timeout_ms`) |
| `"memory"` | The state hit its memory limit (`[lua] memory_limit`) |
| `"panic"` | A Rust panic was contained at the request boundary — a bug, not an application error |

## `on_error` handlers

Register an app-wide handler, a per-route one, or both. The per-route
handler wins where both exist:

```lua
local app = nitr.app()

app:on_error(function(err, req)
  if err.kind == "timeout" then
    return { status = 504, body = "upstream too slow" }
  end
  return { status = 500, json = { error = err.message } }
end)

app:get("/report", handler, {
  on_error = function(err, req)
    return { status = 500, body = "report generation failed" }
  end,
})
```

The handler receives the error table and the original request, and must
return a response table. Rules:

- It runs only for failures *in your handler chain* (the first row of the
  table above). Rust-side rejections such as `404` or `413` never reach
  it — there is no application failure to explain.
- If the error handler itself fails, or returns something that is not a
  valid response, Nitr logs that and falls back to its own error
  response. Error handling never recurses.
- The failure is logged (structured, with `error.kind`, `error.source`,
  `error.line`, `error.module`) *before* your handler runs, so handling
  an error does not hide it from the logs.

## Status codes Nitr produces

Responses your application never sees, answered in Rust:

| Status | When | Notes |
| --- | --- | --- |
| `404` | No route or static mount matched | |
| `405` | The path exists, the method does not | Carries `Allow`; a bare `OPTIONS` on a known path gets `204` + `Allow` instead |
| `408` | The client sent no complete request header within 30 s | Enforced by the HTTP layer per connection |
| `413` | Body beyond `max_body_bytes` (declared or counted while reading) | Also for multipart parts beyond their limits |
| `414` | URI beyond `max_uri_bytes` | |
| `429` | Per-IP budget exceeded (`[rate_limit]`) | Carries `Retry-After` |
| `500` | A handler failure your `on_error` did not answer — including timeout, memory, and contained panics | See the two modes below |
| `503` | No free Lua state within `pool_wait_ms` (carries `Retry-After: 1`), streaming rejected at `max_streams`, or the server is draining | Shed *before* any Lua runs |

## Development versus production

The `500` body has two deliberately different faces, switched by
`dev_mode` (set in `nitr.toml`, or implied by `nitr develop`):

- **Production** answers with exactly `Internal Server Error` — no
  message, no paths, no traceback. Failure details are for the operator,
  and they live in the structured log line, not in what an attacker can
  read by causing errors.
- **Development** renders the error in context: the concise headline
  (`kind: message (source:line)`), the failing source with the line
  marked, the bounded traceback and cause chain — as HTML when the
  client accepts it, plain text otherwise.

Your own `on_error` responses are returned verbatim in both modes;
withholding detail there is your call.

## The configured limits

Every limit that produces one of the statuses above, in one place:

| Key | Section | Default | On violation |
| --- | --- | --- | --- |
| `max_body_bytes` | `[limits]` | 1 MiB | `413` |
| `max_uri_bytes` | `[limits]` | 8 KiB | `414` |
| `max_header_bytes` | `[limits]` | 16 KiB | connection-level rejection |
| `max_connections` | `[limits]` | 1024 | listener stops accepting |
| `pool_wait_ms` | `[limits]` | 5000 | `503` + `Retry-After` |
| `max_form_parts` / `max_field_bytes` / `max_file_bytes` | `[limits]` | 64 / 64 KiB / 10 MiB | `413` |
| `requests` per `window` | `[rate_limit]` | off | `429` + `Retry-After` |
| `exec_timeout_ms` | `[lua]` | 30000 | error with `kind = "timeout"` → `500` |
| `memory_limit` | `[lua]` | 8 MiB | error with `kind = "memory"` → `500` |

## Reporting a bug

A `kind = "panic"` error, a process crash, or a `500` with no
corresponding log line is a Nitr bug. An actionable report includes:

- the structured log line (`error.kind` and friends), and the panic
  message if there is one;
- Nitr's version and how it was built (release binary, `cargo install`,
  distro package);
- the smallest handler that reproduces it, if you can find one — the
  `source`/`line` fields usually point at it.
