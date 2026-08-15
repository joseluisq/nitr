//! End-to-end tests for phase 12: SQLite that behaves under concurrency,
//! migrations, the shared cache, and a `fetch` that can retry, be bounded,
//! and be correlated.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("nitr-data-io-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("scratch dir");
    dir.join(name)
}

fn free_addr() -> SocketAddr {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind port 0");
    listener.local_addr().expect("local addr")
}

async fn wait_until_listening(addr: SocketAddr) {
    for _ in 0..200 {
        if tokio::net::TcpStream::connect(addr).await.is_ok() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("nothing came up on {addr}");
}

/// A Nitr server running the given handler script.
struct Harness {
    addr: SocketAddr,
    client: reqwest::Client,
    stop: tokio::sync::oneshot::Sender<()>,
    served: tokio::task::JoinHandle<nitr::Result>,
}

impl Harness {
    async fn start(mut cfg: nitr::Config, script_name: &str, script: &str) -> Self {
        let handler = scratch(script_name);
        std::fs::write(&handler, script).expect("write handler");
        cfg.handler_script = handler;
        cfg.listen = free_addr();
        cfg.workers = 4;
        cfg.shutdown.grace = 5;
        cfg.shutdown.stream_grace = 0;
        let addr = cfg.listen;

        let server = nitr::Server::builder()
            .config(cfg)
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
            client: reqwest::Client::new(),
            stop,
            served,
        }
    }

    fn url(&self, path: &str) -> String {
        format!("http://{}{path}", self.addr)
    }

    async fn json(&self, path: &str) -> serde_json::Value {
        let resp = self
            .client
            .get(self.url(path))
            .send()
            .await
            .unwrap_or_else(|err| panic!("GET {path}: {err}"));
        let status = resp.status();
        let text = resp.text().await.expect("body");
        assert!(status.is_success(), "GET {path} -> {status}: {text}");
        serde_json::from_str(&text).unwrap_or_else(|err| panic!("GET {path} json: {err}: {text}"))
    }

    async fn stop(self) {
        let _ = self.stop.send(());
        self.served
            .await
            .expect("server task")
            .expect("clean shutdown");
    }
}

/// A stub upstream for the `fetch` tests.
///
/// Records what it received and can be told to fail the first N requests,
/// which is what makes retry behavior observable rather than assumed.
#[derive(Clone, Default)]
struct Upstream {
    requests: Arc<AtomicUsize>,
    fail_first: Arc<AtomicUsize>,
    traceparents: Arc<Mutex<Vec<Option<String>>>>,
}

impl Upstream {
    async fn start(&self) -> SocketAddr {
        use http_body_util::Full;
        use hyper::body::Bytes;
        use hyper::service::service_fn;
        use hyper::{Response, StatusCode};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind upstream");
        let addr = listener.local_addr().expect("upstream addr");
        let state = self.clone();

        tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    break;
                };
                let state = state.clone();
                tokio::spawn(async move {
                    let service = service_fn(move |req: hyper::Request<hyper::body::Incoming>| {
                        let state = state.clone();
                        async move {
                            state.requests.fetch_add(1, Ordering::SeqCst);
                            state.traceparents.lock().expect("lock").push(
                                req.headers()
                                    .get("traceparent")
                                    .and_then(|v| v.to_str().ok())
                                    .map(str::to_string),
                            );
                            // Fail the first N, then succeed: a retry that
                            // works has to be visible as a later success.
                            let remaining = state.fail_first.load(Ordering::SeqCst);
                            if remaining > 0 {
                                state.fail_first.store(remaining - 1, Ordering::SeqCst);
                                return Ok::<_, std::convert::Infallible>(
                                    Response::builder()
                                        .status(StatusCode::INTERNAL_SERVER_ERROR)
                                        .body(Full::new(Bytes::from_static(b"nope")))
                                        .expect("response"),
                                );
                            }
                            Ok(Response::new(Full::new(Bytes::from_static(
                                b"{\"ok\":true}",
                            ))))
                        }
                    });
                    let _ = hyper::server::conn::http1::Builder::new()
                        .serve_connection(hyper_util::rt::TokioIo::new(stream), service)
                        .await;
                });
            }
        });
        wait_until_listening(addr).await;
        addr
    }

    fn seen(&self) -> usize {
        self.requests.load(Ordering::SeqCst)
    }
}

// ---------------------------------------------------------------------------

const DB_SCRIPT: &str = r#"
local app = nitr.app()

app:get("/pragmas", function(req)
    local journal = nitr.db:query_row("PRAGMA journal_mode")
    local fk = nitr.db:query_row("PRAGMA foreign_keys")
    local busy = nitr.db:query_row("PRAGMA busy_timeout")
    return nitr.json({
        journal_mode = journal.journal_mode,
        foreign_keys = fk.foreign_keys,
        busy_timeout = busy.timeout,
    })
end)

-- Every request writes; with the default rollback journal and no busy
-- timeout, concurrent calls to this would fail with SQLITE_BUSY.
app:get("/write", function(req)
    nitr.db:execute("INSERT INTO counters (value) VALUES (?)", { req.id })
    local row = nitr.db:query_row("SELECT COUNT(*) AS n FROM counters")
    return nitr.json({ n = row.n })
end)

-- The footgun: using the outer handle inside a transaction body.
app:get("/footgun", function(req)
    local ok, err = pcall(function()
        nitr.db:transaction(function(tx)
            tx:execute("INSERT INTO counters (value) VALUES ('inside')")
            -- This would silently join the transaction.
            nitr.db:execute("INSERT INTO counters (value) VALUES ('escaped')")
        end)
    end)
    return nitr.json({ ok = ok, err = tostring(err) })
end)

-- A foreign key that SQLite would ignore without the pragma.
app:get("/foreign-key", function(req)
    local ok, err = pcall(function()
        nitr.db:execute("INSERT INTO children (parent_id) VALUES (9999)")
    end)
    return nitr.json({ ok = ok, err = tostring(err) })
end)

return app
"#;

fn db_config(name: &str) -> (nitr::Config, PathBuf) {
    let path = scratch(name);
    for suffix in ["", "-wal", "-shm"] {
        let _ = std::fs::remove_file(format!("{}{suffix}", path.display()));
    }
    let mut cfg = nitr::Config {
        database: Some(nitr::DatabaseConfig::new(&path)),
        ..Default::default()
    };
    cfg.std.features = Some(vec!["json".into(), "http".into(), "db".into()]);
    (cfg, path)
}

fn seed(path: &PathBuf) {
    let conn = rusqlite::Connection::open(path).expect("open");
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS counters (id INTEGER PRIMARY KEY, value TEXT);
         CREATE TABLE IF NOT EXISTS parents (id INTEGER PRIMARY KEY);
         CREATE TABLE IF NOT EXISTS children (
             id INTEGER PRIMARY KEY,
             parent_id INTEGER REFERENCES parents(id)
         );",
    )
    .expect("seed schema");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sqlite_runs_with_the_pragmas_a_server_needs() {
    let (cfg, path) = db_config("pragmas.db");
    seed(&path);
    let h = Harness::start(cfg, "db_app_pragmas.lua", DB_SCRIPT).await;

    let body = h.json("/pragmas").await;
    assert_eq!(body["journal_mode"], "wal");
    assert_eq!(body["foreign_keys"], 1);
    assert_eq!(body["busy_timeout"], 5000);

    // Foreign keys are actually enforced, which SQLite does not do by
    // default however the schema is written.
    let body = h.json("/foreign-key").await;
    assert_eq!(body["ok"], false, "the constraint must be enforced");
    assert!(
        body["err"].as_str().expect("err").contains("FOREIGN KEY"),
        "{}",
        body["err"]
    );

    h.stop().await;
}

/// The failure mode this phase exists to remove: several pooled states
/// writing at once. On the old defaults one of them gets `SQLITE_BUSY`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_writers_do_not_collide() {
    let (cfg, path) = db_config("concurrent.db");
    seed(&path);
    let h = Harness::start(cfg, "db_app_concurrent.lua", DB_SCRIPT).await;

    let mut requests = Vec::new();
    for _ in 0..40 {
        let client = h.client.clone();
        let url = h.url("/write");
        requests.push(tokio::spawn(async move {
            client.get(url).send().await?.status().as_u16().pipe_ok()
        }));
    }
    for handle in requests {
        let status = handle.await.expect("task").expect("request");
        assert_eq!(status, 200, "a concurrent write failed");
    }

    let body = h.json("/write").await;
    assert_eq!(body["n"], 41, "every write must have landed");

    h.stop().await;
}

/// Small helper so the concurrency test above reads cleanly.
trait PipeOk: Sized {
    fn pipe_ok(self) -> reqwest::Result<Self> {
        Ok(self)
    }
}
impl PipeOk for u16 {}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_outer_handle_refuses_to_run_inside_a_transaction() {
    let (cfg, path) = db_config("footgun.db");
    seed(&path);
    let h = Harness::start(cfg, "db_app_footgun.lua", DB_SCRIPT).await;

    let body = h.json("/footgun").await;
    assert_eq!(body["ok"], false, "the escape must be an error now");
    let err = body["err"].as_str().expect("err");
    assert!(err.contains("transaction is open"), "{err}");

    // The transaction rolled back, so neither row is there — including the
    // one the body wrote before the mistake.
    let conn = rusqlite::Connection::open(&path).expect("open");
    let count: i64 = conn
        .query_row("SELECT COUNT(*) FROM counters", [], |row| row.get(0))
        .expect("count");
    assert_eq!(count, 0);

    h.stop().await;
}

// ---------------------------------------------------------------------------

#[test]
fn migrations_apply_and_the_ledger_is_readable() {
    let dir = scratch("migrations");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("migrations dir");
    std::fs::write(
        dir.join("001_create_notes.sql"),
        "CREATE TABLE notes (id INTEGER PRIMARY KEY, body TEXT NOT NULL);",
    )
    .expect("write 001");
    std::fs::write(
        dir.join("002_add_index.sql"),
        "CREATE INDEX notes_body ON notes (body);",
    )
    .expect("write 002");

    let path = scratch("migrated.db");
    for suffix in ["", "-wal", "-shm"] {
        let _ = std::fs::remove_file(format!("{}{suffix}", path.display()));
    }
    let pragmas = nitr::stdlib::SqlitePragmas::default();
    let conn = nitr::stdlib::db_open(&path, &pragmas).expect("open");

    assert_eq!(
        nitr::stdlib::migrate::pending(&conn, &dir).expect("pending"),
        vec!["001_create_notes.sql", "002_add_index.sql"]
    );
    let applied = nitr::stdlib::migrate::run(&conn, &dir).expect("run");
    assert_eq!(applied.len(), 2);
    assert!(nitr::stdlib::migrate::pending(&conn, &dir)
        .expect("pending")
        .is_empty());

    conn.execute("INSERT INTO notes (body) VALUES ('hi')", [])
        .expect("the schema really exists");
}

/// Applying schema changes at boot is how a rolling deployment races
/// itself, so a pending migration stops the server instead.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_pending_migration_refuses_the_boot() {
    let dir = scratch("pending-migrations");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("migrations dir");
    std::fs::write(
        dir.join("001_create_things.sql"),
        "CREATE TABLE things (id INTEGER PRIMARY KEY);",
    )
    .expect("write 001");

    let (mut cfg, _path) = db_config("pending.db");
    cfg.database.as_mut().expect("database").migrations_dir = Some(dir.clone());
    let handler = scratch("db_app_pending.lua");
    std::fs::write(&handler, DB_SCRIPT).expect("write handler");
    cfg.handler_script = handler;
    cfg.listen = free_addr();

    let err = nitr::Server::builder()
        .config(cfg.clone())
        .build()
        .await
        .expect_err("must refuse to start");
    let message = err.to_string();
    assert!(message.contains("001_create_things.sql"), "{message}");
    assert!(message.contains("nitr migrate"), "{message}");

    // Once applied, the same configuration starts.
    let db = cfg.database.clone().expect("database");
    let conn = nitr::stdlib::db_open(&db.path, &db.pragmas()).expect("open");
    nitr::stdlib::migrate::run(&conn, &dir).expect("migrate");
    drop(conn);

    nitr::Server::builder()
        .config(cfg)
        .build()
        .await
        .expect("starts once the schema is current");
}

// ---------------------------------------------------------------------------

const CACHE_SCRIPT: &str = r#"
local app = nitr.app()

-- Counts how often the expensive function actually ran *in this state*.
local computed = 0

app:get("/remember", function(req)
    local value = nitr.cache:remember("rates", { ttl = 60 }, function()
        computed = computed + 1
        return { usd = 1.0, eur = 0.92 }
    end)
    return nitr.json({ value = value, computed_here = computed })
end)

app:get("/set", function(req)
    nitr.cache:set("shared", { who = req.query.who }, { ttl = 60 })
    return nitr.json({ ok = true })
end)

app:get("/get", function(req)
    return nitr.json({ value = nitr.cache:get("shared") })
end)

app:get("/stats", function(req)
    return nitr.json(nitr.cache:stats())
end)

app:get("/uncacheable", function(req)
    local ok, err = pcall(function()
        nitr.cache:set("fn", function() end)
    end)
    return nitr.json({ ok = ok, err = tostring(err) })
end)

return app
"#;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_cache_is_shared_across_states_and_bounded() {
    let mut cfg = nitr::Config::default();
    cfg.std.features = Some(vec!["json".into(), "http".into(), "cache".into()]);
    let h = Harness::start(cfg, "cache_app.lua", CACHE_SCRIPT).await;

    // Written by whichever state served this, read back by (very likely) a
    // different one: the whole point is that they share the storage.
    h.json("/set?who=alice").await;
    for _ in 0..8 {
        let body = h.json("/get").await;
        assert_eq!(body["value"]["who"], "alice");
    }

    // `remember` runs the function once for the whole pool, not once per
    // state, because the value is in the shared cache after the first call.
    let mut total_computed = 0;
    for _ in 0..12 {
        let body = h.json("/remember").await;
        assert_eq!(body["value"]["eur"], 0.92);
        total_computed += body["computed_here"].as_u64().expect("computed");
    }
    let stats = h.json("/stats").await;
    assert!(
        stats["hits"].as_u64().expect("hits") > 0,
        "the second and later reads must be hits: {stats}"
    );
    assert!(
        total_computed <= 12,
        "the expensive function must not run every time"
    );

    // A function cannot be cached: entries are plain data, which is what
    // keeps one state from reaching into another's heap.
    let body = h.json("/uncacheable").await;
    assert_eq!(body["ok"], false);
    assert!(
        body["err"].as_str().expect("err").contains("plain data"),
        "{}",
        body["err"]
    );

    h.stop().await;
}

// ---------------------------------------------------------------------------

const FETCH_SCRIPT: &str = r#"
local app = nitr.app()

app:get("/retry", function(req)
    local resp = nitr.fetch("get", nitr.cfg.upstream, {
        retry = { attempts = 4, backoff = "constant" },
    }):send()
    return nitr.json({ status = resp.status, body = resp:json() })
end)

-- POST is never repeated automatically, whatever the caller asks for.
app:get("/retry-post", function(req)
    local ok, err = pcall(function()
        return nitr.fetch("post", nitr.cfg.upstream, {
            body = "x",
            retry = { attempts = 4 },
        }):send()
    end)
    return nitr.json({ ok = ok, err = tostring(err) })
end)

app:get("/budget", function(req)
    local made, err = 0, nil
    for _ = 1, 10 do
        local ok, e = pcall(function()
            nitr.fetch("get", nitr.cfg.upstream):send()
        end)
        if not ok then err = tostring(e) break end
        made = made + 1
    end
    return nitr.json({ made = made, err = tostring(err) })
end)

app:get("/traced", function(req)
    local resp = nitr.fetch("get", nitr.cfg.upstream):send()
    return nitr.json({ status = resp.status })
end)

app:get("/private", function(req)
    local ok, err = pcall(function()
        nitr.fetch("get", "http://localhost:9/"):send()
    end)
    return nitr.json({ ok = ok, err = tostring(err) })
end)

return app
"#;

async fn fetch_harness(upstream: SocketAddr, tune: impl FnOnce(&mut nitr::Config)) -> Harness {
    // Tests run in parallel, so every script path is keyed by the upstream
    // port: a shared name would have one test reading another's config.
    let port = upstream.port();
    let config_script = scratch(&format!("fetch_config_{port}.lua"));
    std::fs::write(
        &config_script,
        format!("return function() return {{ upstream = \"http://{upstream}/\" }} end"),
    )
    .expect("write config script");

    let mut cfg = nitr::Config {
        config_script: Some(config_script),
        ..Default::default()
    };
    cfg.std.features = Some(vec!["json".into(), "http".into(), "fetch".into()]);
    // The stub upstream is on loopback, which the SSRF policy forbids by
    // default — exactly as it should.
    cfg.fetch.allow_private_networks = true;
    tune(&mut cfg);
    Harness::start(cfg, &format!("fetch_app_{port}.lua"), FETCH_SCRIPT).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn idempotent_requests_retry_and_others_do_not() {
    let upstream = Upstream::default();
    let addr = upstream.start().await;
    upstream.fail_first.store(2, Ordering::SeqCst);

    let h = fetch_harness(addr, |_| {}).await;

    let body = h.json("/retry").await;
    assert_eq!(body["status"], 200, "the third attempt must succeed");
    assert_eq!(body["body"]["ok"], true);
    assert_eq!(upstream.seen(), 3, "two failures plus the success");

    // A POST is sent exactly once even though the caller asked for four
    // attempts: repeating it is how a customer gets charged twice.
    upstream.fail_first.store(3, Ordering::SeqCst);
    let before = upstream.seen();
    h.json("/retry-post").await;
    assert_eq!(
        upstream.seen() - before,
        1,
        "a POST must never be repeated automatically"
    );

    h.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn one_inbound_request_has_a_bounded_outbound_cost() {
    let upstream = Upstream::default();
    let addr = upstream.start().await;

    let h = fetch_harness(addr, |cfg| cfg.fetch.max_per_request = 3).await;

    let body = h.json("/budget").await;
    assert_eq!(body["made"], 3, "the fourth call must be refused");
    assert!(
        body["err"]
            .as_str()
            .expect("err")
            .contains("max_per_request"),
        "{}",
        body["err"]
    );

    // The next inbound request starts with a fresh budget.
    let body = h.json("/budget").await;
    assert_eq!(body["made"], 3);

    h.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn trace_context_is_forwarded_when_enabled() {
    let upstream = Upstream::default();
    let addr = upstream.start().await;

    let h = fetch_harness(addr, |cfg| cfg.fetch.propagate_trace_context = true).await;
    h.json("/traced").await;
    h.stop().await;

    let seen = upstream.traceparents.lock().expect("lock").clone();
    let traceparent = seen
        .last()
        .expect("one request")
        .clone()
        .expect("traceparent must be present");
    // version-traceid-spanid-flags, with the ids the documented widths.
    let parts: Vec<&str> = traceparent.split('-').collect();
    assert_eq!(parts.len(), 4, "malformed traceparent `{traceparent}`");
    assert_eq!(parts[0], "00");
    assert_eq!(parts[1].len(), 32, "trace id must be 16 bytes");
    assert_eq!(parts[2].len(), 16, "span id must be 8 bytes");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_ssrf_policy_still_refuses_loopback_by_default() {
    let upstream = Upstream::default();
    let addr = upstream.start().await;

    // Note the override is *off* here, unlike the other fetch tests.
    let h = fetch_harness(addr, |cfg| cfg.fetch.allow_private_networks = false).await;
    let body = h.json("/private").await;
    assert_eq!(body["ok"], false);
    assert!(
        body["err"].as_str().expect("err").contains("private"),
        "{}",
        body["err"]
    );
    h.stop().await;
}

// ---------------------------------------------------------------------------

const AWAIT_SCRIPT: &str = r#"
local app = nitr.app()

app:get("/combined", function(req)
    -- A query and an HTTP call at the same time, rather than one after the
    -- other: this is what db:query_async exists for.
    local rows, resp = nitr.await_all(
        nitr.db:query_async("SELECT value FROM counters ORDER BY id"),
        nitr.fetch("get", nitr.cfg.upstream)
    )
    return nitr.json({ rows = rows, upstream = resp:json() })
end)

app:get("/reuse", function(req)
    local handle = nitr.db:query_async("SELECT 1 AS n")
    nitr.await_all(handle)
    local ok, err = pcall(function() nitr.await_all(handle) end)
    return nitr.json({ ok = ok, err = tostring(err) })
end)

return app
"#;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_query_and_a_fetch_can_run_together() {
    let upstream = Upstream::default();
    let addr = upstream.start().await;

    let (mut cfg, path) = db_config("await.db");
    seed(&path);
    {
        let conn = rusqlite::Connection::open(&path).expect("open");
        conn.execute("INSERT INTO counters (value) VALUES ('one')", [])
            .expect("seed row");
    }
    let config_script = scratch("await_config.lua");
    std::fs::write(
        &config_script,
        format!("return function() return {{ upstream = \"http://{addr}/\" }} end"),
    )
    .expect("write config script");
    cfg.config_script = Some(config_script);
    cfg.std.features = Some(vec![
        "json".into(),
        "http".into(),
        "db".into(),
        "fetch".into(),
    ]);
    cfg.fetch.allow_private_networks = true;

    let h = Harness::start(cfg, "await_app.lua", AWAIT_SCRIPT).await;

    let body = h.json("/combined").await;
    assert_eq!(body["rows"][0]["value"], "one");
    assert_eq!(body["upstream"]["ok"], true);

    // A handle is one-shot: awaiting it twice is a mistake worth naming.
    let body = h.json("/reuse").await;
    assert_eq!(body["ok"], false);
    assert!(
        body["err"].as_str().expect("err").contains("already"),
        "{}",
        body["err"]
    );

    h.stop().await;
}
