# Log schema

What Nitr's logs contain and promise. Output format and level come from
`[log]` in `nitr.toml` (`format = "text" | "json"`, `level`), with the
`RUST_LOG` environment variable winning over the configured level.
`format = "json"` emits one JSON object per line with every field below as
a real key, ready for a log shipper.

## Spans

Nitr instruments the request path with a small fixed span hierarchy.
A span contributes in two ways: every log event emitted inside it carries
its fields as context, and the span itself emits one *close* line when it
ends, carrying its fields plus `time.busy`/`time.idle`.

| Span | Level | Opened around | Fields |
| ---- | ----- | ------------- | ------ |
| `request` | INFO | the whole request, dispatch to response | `id`, `method`, `path`, `status` (recorded at completion) |
| `pool_checkout` | DEBUG | waiting for a free Lua state | `wait_ms`, `outcome` (`hit` / `shed`) |
| `lua_handler` | DEBUG | the script's middleware+handler chain | `elapsed_ms` |
| `db_query` | DEBUG | one SQL statement (`nitr.db`) | `kind` (`query` / `query_row` / `query_one` / `execute` / `tx`), `elapsed_ms` |
| `fetch` | DEBUG | one outbound network exchange (`nitr.fetch`) | `host`, `method`, `status`, `ip`, `elapsed_ms` |

Everything nests under `request`, so any line — including `nitr.log.*`
calls from Lua and a `fetch` SSRF denial — arrives already correlated with
the request id, method, and path.

At the default `info` level exactly one close line appears per request:
the `request` span, which reads as an access-log entry. At `debug` (the
dev-mode default) the inner spans appear too and decompose where the time
went: pool wait vs. script execution vs. database vs. upstream calls.
`level = "warn"` silences the spans wholesale; there is no separate
switch.

Notes on individual spans:

- `pool_checkout`'s `outcome = "shed"` pairs with the 503 the request was
  answered with; a damaged state being replaced logs a separate
  `outcome = "rebuilt"` event from the pool.
- `fetch` opens one span per network exchange, so a call that redirects
  twice (or retries) produces one span per hop, each with the status that
  hop answered. `ip` is the address the SSRF-vetted resolution actually
  connected to — the security-relevant fact for an audit trail.
- `elapsed_ms`/`wait_ms` are explicit integer fields; prefer them over
  parsing the human-formatted `time.busy`/`time.idle`.

## Redaction rules

Enforced by review; the vocabulary of span fields is deliberately closed:

- **No SQL text and no bind values** — statements can embed secrets, and
  logs outlive them. `db_query` carries only the statement kind and
  duration.
- **No full URLs** — query strings carry tokens. `fetch` carries the host
  (and connected IP), never the path or query.
- **No header values, no cookie or session material** anywhere.
- Hosts, methods, status codes, ids, durations, and counts are the whole
  vocabulary. A new span field must fit that list or state why not.

Application logs (`nitr.log.*`) are the script author's responsibility;
these rules bind what *Nitr itself* emits.
