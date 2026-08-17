//! End-to-end tests for phase 13: the error model and diagnostics.
//!
//! Structured error values in `on_error`, per-route error handlers,
//! classification, the dev/production presentation split, and load-time
//! diagnostics that point at the offending line.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

/// The handler script: `/boom` fails at a known line, and the app-wide
/// `on_error` reports every structured field back as JSON so tests can
/// assert on them from outside.
const APP_SCRIPT: &str = r#"
local app = nitr.app()

app:get("/ok", function(req)
    return nitr.text("ok")
end)

app:get("/boom", function(req)
    local user = nil
    return nitr.text(user.name)
end)

app:get("/spin", function(req)
    while true do end
end)

app:get("/routed", function(req)
    error("routed failure")
end, { on_error = function(err, req)
    return { status = 500, body = "route handled: " .. err.kind }
end })

-- `nitr.errinfo` classifies whatever pcall caught: a Lua error (a string
-- with a position prefix) and a Rust builtin error (full chain) alike.
app:get("/caught", function(req)
    local ok1, lua_err = pcall(function() local x = nil; return x.y end)
    local ok2, rust_err = pcall(function() return nitr.json:decode("{not json") end)
    assert(not ok2, "decode of invalid JSON must fail")
    local le = nitr.errinfo(lua_err)
    local re = nitr.errinfo(rust_err)
    return nitr.json({
        lua_kind = le.kind,
        lua_line = le.line,
        lua_source = le.source,
        rust_kind = re.kind,
        rust_message = re.message,
        concat = ("prefix: " .. le),
        pretty = le.pretty,
    })
end)

app:on_error(function(err, req)
    return {
        status = 500,
        headers = { ["Content-Type"] = "application/json" },
        body = nitr.json:encode({
            message = err.message,
            kind = err.kind,
            source = err.source,
            line = err.line,
            has_traceback = err.traceback ~= nil,
            as_string = tostring(err),
        }),
    }
end)

return app
"#;

fn write_temp_script(name: &str, content: &str) -> PathBuf {
    // `fs::write` truncates before writing, so a path two tests share is a
    // race; the counter keeps every call on its own file.
    static NEXT: AtomicU32 = AtomicU32::new(0);
    let id = NEXT.fetch_add(1, Ordering::Relaxed);
    let path = std::env::temp_dir().join(format!(
        "nitr-diagnostics-{}-{id}-{name}",
        std::process::id()
    ));
    std::fs::write(&path, content).expect("write temp script");
    path
}

/// Binds port 0 (the OS picks a free port) and keeps the listener alive.
/// The server adopts it via `.listener(...)`, so the port can never be
/// taken by another test between choosing it and serving on it.
/// Removes ANSI SGR sequences (`ESC [ … m`), leaving the visible text.
fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' {
            // Skip to the terminating `m` of the escape sequence.
            for c in chars.by_ref() {
                if c == 'm' {
                    break;
                }
            }
        } else {
            out.push(c);
        }
    }
    out
}

fn reserve_addr() -> (std::net::TcpListener, SocketAddr) {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind port 0");
    let addr = listener.local_addr().expect("local addr");
    (listener, addr)
}

async fn wait_until_listening(addr: SocketAddr) {
    for _ in 0..200 {
        if tokio::net::TcpStream::connect(addr).await.is_ok() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("the server never started listening on {addr}");
}

struct Harness {
    addr: SocketAddr,
    handler: PathBuf,
    stop: tokio::sync::oneshot::Sender<()>,
    served: tokio::task::JoinHandle<nitr::Result>,
}

impl Harness {
    async fn start(script: &str, dev_mode: bool, tune: impl FnOnce(&mut nitr::Config)) -> Self {
        let handler = write_temp_script("app.lua", script);
        let (listener, addr) = reserve_addr();
        let mut cfg = nitr::Config {
            listen: addr,
            workers: 1,
            ..Default::default()
        };
        cfg.shutdown.grace = 5;
        cfg.shutdown.stream_grace = 0;
        tune(&mut cfg);
        let server = nitr::Server::builder()
            .config(cfg)
            .listener(listener)
            .handler_script(&handler)
            .builtins(nitr::Builtins::JSON | nitr::Builtins::HTTP)
            .dev_mode(dev_mode)
            .build()
            .await
            .expect("build server");
        let (stop, stop_rx) = tokio::sync::oneshot::channel::<()>();
        let served = tokio::spawn(server.serve_with_shutdown(async {
            let _ = stop_rx.await;
        }));
        wait_until_listening(addr).await;
        Self {
            addr,
            handler,
            stop,
            served,
        }
    }

    fn url(&self, path: &str) -> String {
        format!("http://{}{path}", self.addr)
    }

    async fn stop(self) {
        let _ = self.stop.send(());
        self.served
            .await
            .expect("server task")
            .expect("clean shutdown");
        std::fs::remove_file(&self.handler).ok();
    }
}

// ---------------------------------------------------------------------------

/// `on_error` receives the classified error as a table: message, kind,
/// source, line, and traceback — and it still stringifies.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn on_error_receives_structured_fields() {
    let h = Harness::start(APP_SCRIPT, false, |_| {}).await;
    let client = reqwest::Client::new();

    let resp = client.get(h.url("/boom")).send().await.expect("GET /boom");
    assert_eq!(resp.status(), 500);
    let err: serde_json::Value = resp.json().await.expect("error json");

    assert_eq!(err["kind"], "lua");
    assert!(
        err["message"]
            .as_str()
            .expect("message")
            .contains("attempt to index a nil value"),
        "unexpected message: {err}"
    );
    // The source is the handler script (its chunk is named after the file),
    // and the line is where `/boom` dereferences nil.
    assert!(
        err["source"].as_str().expect("source").contains("app.lua"),
        "unexpected source: {err}"
    );
    assert_eq!(err["line"], 10);
    assert_eq!(err["has_traceback"], true);
    // tostring(err) keeps string-shaped usage working, in the concise form.
    let as_string = err["as_string"].as_str().expect("as_string");
    assert!(as_string.starts_with("lua:"), "got: {as_string}");
    assert!(as_string.contains("app.lua:10"), "got: {as_string}");

    h.stop().await;
}

/// A per-route `on_error` wins over the app-wide handler.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn per_route_on_error_overrides_app_handler() {
    let h = Harness::start(APP_SCRIPT, false, |_| {}).await;
    let client = reqwest::Client::new();

    let resp = client
        .get(h.url("/routed"))
        .send()
        .await
        .expect("GET /routed");
    assert_eq!(resp.status(), 500);
    assert_eq!(resp.text().await.expect("body"), "route handled: lua");

    h.stop().await;
}

/// A CPU-bound overrun is classified `timeout`, not a script bug.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn budget_overruns_are_classified_as_timeouts() {
    let h = Harness::start(APP_SCRIPT, false, |cfg| {
        cfg.lua.exec_timeout_ms = 200;
    })
    .await;
    let client = reqwest::Client::new();

    let resp = client.get(h.url("/spin")).send().await.expect("GET /spin");
    assert_eq!(resp.status(), 500);
    let err: serde_json::Value = resp.json().await.expect("error json");
    assert_eq!(err["kind"], "timeout", "got: {err}");

    h.stop().await;
}

/// `nitr.errinfo` classifies pcall-caught errors — Lua strings and Rust
/// builtin errors alike — and the value concatenates as the concise line.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn errinfo_classifies_caught_errors() {
    let h = Harness::start(APP_SCRIPT, false, |_| {}).await;
    let client = reqwest::Client::new();

    let resp = client
        .get(h.url("/caught"))
        .send()
        .await
        .expect("GET /caught");
    assert_eq!(resp.status(), 200);
    let out: serde_json::Value = resp.json().await.expect("json");

    // The Lua error keeps its position through the string round trip.
    assert_eq!(out["lua_kind"], "lua", "got: {out}");
    assert!(
        out["lua_source"]
            .as_str()
            .expect("lua_source")
            .contains("app.lua"),
        "got: {out}"
    );
    assert!(
        out["lua_line"].as_u64().is_some_and(|l| l > 0),
        "got: {out}"
    );
    // The Rust builtin error is classified as a boundary failure.
    assert_eq!(out["rust_kind"], "nitr", "got: {out}");
    // `__concat` renders the concise form directly into a string.
    let concat = out["concat"].as_str().expect("concat");
    assert!(concat.starts_with("prefix: lua:"), "got: {concat}");
    assert!(concat.contains("app.lua"), "got: {concat}");
    // `pretty` is the concise form, ANSI-colored exactly when the server
    // process writes to a terminal with NO_COLOR unset. The server runs in
    // this test process, so compute the same gate here instead of assuming
    // one environment: `cargo test` in an interactive terminal keeps the
    // real stdout fd, so the gate is genuinely open there.
    use std::io::IsTerminal as _;
    let colored = std::io::stdout().is_terminal()
        && std::env::var_os("NO_COLOR").is_none_or(|v| v.is_empty());
    let pretty = out["pretty"].as_str().expect("pretty");
    assert_eq!(pretty.contains('\u{1b}'), colored, "got: {pretty:?}");
    let plain = strip_ansi(pretty);
    assert!(plain.starts_with("lua:"), "got: {plain}");
    assert!(plain.contains("app.lua"), "got: {plain}");

    h.stop().await;
}

/// Production responses stay curt: no source paths, no tracebacks, no
/// internal detail — the structured log line is where the diagnosis lives.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn production_responses_leak_no_source() {
    // No on_error handler, so the built-in error response answers.
    let script = r#"
local app = nitr.app()
app:get("/boom", function(req)
    local user = nil
    return nitr.text(user.name)
end)
return app
"#;
    let h = Harness::start(script, false, |_| {}).await;
    let client = reqwest::Client::new();

    let resp = client.get(h.url("/boom")).send().await.expect("GET /boom");
    assert_eq!(resp.status(), 500);
    let body = resp.text().await.expect("body");
    assert_eq!(body, "Internal Server Error");

    h.stop().await;
}

/// Development mode renders the error in context: the failing line marked
/// in its source, the traceback, and the concise headline.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dev_mode_shows_the_failing_source_line() {
    let script = r#"
local app = nitr.app()
app:get("/boom", function(req)
    local user = nil
    return nitr.text(user.name)
end)
return app
"#;
    let h = Harness::start(script, true, |_| {}).await;
    let client = reqwest::Client::new();

    let resp = client.get(h.url("/boom")).send().await.expect("GET /boom");
    assert_eq!(resp.status(), 500);
    let body = resp.text().await.expect("body");
    assert!(body.contains("attempt to index a nil value"), "got: {body}");
    assert!(body.contains("app.lua:5"), "got: {body}");
    // The source snippet marks the failing line.
    assert!(
        body.contains("5 |     return nitr.text(user.name)"),
        "got: {body}"
    );
    assert!(body.contains("stack traceback:"), "got: {body}");

    // A browser gets the same content as HTML.
    let resp = client
        .get(h.url("/boom"))
        .header("accept", "text/html")
        .send()
        .await
        .expect("GET /boom html");
    assert!(
        resp.headers()["content-type"]
            .to_str()
            .expect("content type")
            .starts_with("text/html"),
    );
    let body = resp.text().await.expect("html body");
    assert!(body.contains("<pre>"), "got: {body}");
    assert!(body.contains("attempt to index a nil value"), "got: {body}");

    h.stop().await;
}

/// A duplicate route names both registration sites: knowing only the
/// second means hunting the file for the first.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn duplicate_routes_name_both_sites() {
    let script = r#"
local app = nitr.app()
app:get("/x", function(req) return { status = 200 } end)
app:get("/x", function(req) return { status = 200 } end)
return app
"#;
    let handler = write_temp_script("dup.lua", script);
    let err = nitr::Server::builder()
        .listen("127.0.0.1:0".parse().expect("addr"))
        .handler_script(&handler)
        .builtins(nitr::Builtins::JSON)
        .workers(1)
        .build()
        .await
        .expect_err("duplicate route must fail the build");
    let message = err.to_string();
    assert!(message.contains("duplicate route `GET /x`"), "{message}");
    assert!(message.contains("first registered here"), "{message}");
    assert!(message.contains("registered again here"), "{message}");
    // Both sites carry the line numbers of the two app:get calls.
    assert!(message.contains(":3"), "{message}");
    assert!(message.contains(":4"), "{message}");
    std::fs::remove_file(&handler).ok();
}

/// A syntax error points at the line, with the source rendered around it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn syntax_errors_point_at_the_line() {
    let script = "local app = nitr.app()\nlocal x =\nreturn app\n";
    let handler = write_temp_script("syntax.lua", script);
    let err = nitr::Server::builder()
        .listen("127.0.0.1:0".parse().expect("addr"))
        .handler_script(&handler)
        .builtins(nitr::Builtins::JSON)
        .workers(1)
        .build()
        .await
        .expect_err("syntax error must fail the build");
    let message = err.to_string();
    assert!(message.contains("-->"), "{message}");
    assert!(message.contains("syntax.lua"), "{message}");
    // The gutter renders the offending source line.
    assert!(message.contains("| return app"), "{message}");
    std::fs::remove_file(&handler).ok();
}
