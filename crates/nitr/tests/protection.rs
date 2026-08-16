//! End-to-end tests for phase-5 observability + protection: request ids
//! (generated and trusted), the `nitr.log` builtin, rate limiting, and the
//! URI/body size limits.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};

const APP_SCRIPT: &str = r#"
local app = nitr.app()

app:get("/", function(req)
    nitr.log.info("handling request", { path = req.path })
    return nitr.json({ id = req.id })
end)

app:post("/upload", function(req)
    return nitr.text("ok")
end)

return app
"#;

fn write_temp_script(name: &str, content: &str) -> PathBuf {
    // `fs::write` truncates before writing, so a path two tests share is a
    // race; the counter keeps every call on its own file.
    static NEXT: AtomicU32 = AtomicU32::new(0);
    let id = NEXT.fetch_add(1, Ordering::Relaxed);
    let path =
        std::env::temp_dir().join(format!("nitr-protect-{}-{id}-{name}", std::process::id()));
    std::fs::write(&path, content).expect("write temp script");
    path
}

/// Binds port 0 (the OS picks a free port) and keeps the listener alive.
/// The server adopts it via `.listener(...)`, so the port can never be
/// taken by another test between choosing it and serving on it.
fn reserve_addr() -> (std::net::TcpListener, SocketAddr) {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind port 0");
    let addr = listener.local_addr().expect("local addr");
    (listener, addr)
}

async fn start(
    cfg: nitr::Config,
    handler: &PathBuf,
) -> (
    SocketAddr,
    tokio::sync::oneshot::Sender<()>,
    tokio::task::JoinHandle<nitr::Result>,
) {
    let (listener, addr) = reserve_addr();
    let server = nitr::Server::builder()
        .config(cfg)
        .listen(addr)
        .listener(listener)
        .handler_script(handler)
        .builtins(nitr::Builtins::JSON | nitr::Builtins::HTTP | nitr::Builtins::LOG)
        .workers(1)
        .build()
        .await
        .expect("build server");
    let (stop_tx, stop_rx) = tokio::sync::oneshot::channel::<()>();
    let served = tokio::spawn(server.serve_with_shutdown(async {
        let _ = stop_rx.await;
    }));
    (addr, stop_tx, served)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn request_ids_and_size_limits() {
    let handler = write_temp_script("app.lua", APP_SCRIPT);

    let mut cfg = nitr::Config::default();
    cfg.limits.max_uri_bytes = 128;
    cfg.limits.max_body_bytes = 1024;

    let (addr, stop_tx, served) = start(cfg, &handler).await;
    let base = format!("http://{addr}");
    let client = reqwest::Client::new();

    // Every response carries a generated X-Request-ID; ids are unique, the
    // handler sees the same id (via req.id), and an inbound id is ignored
    // by default (untrusted).
    let resp = client
        .get(format!("{base}/"))
        .header("x-request-id", "spoofed-id")
        .send()
        .await
        .expect("GET /");
    let id1 = resp.headers()["x-request-id"]
        .to_str()
        .expect("id header")
        .to_string();
    assert_ne!(id1, "spoofed-id");
    let body: serde_json::Value = resp.json().await.expect("body");
    assert_eq!(body["id"], id1.as_str());

    let resp = client.get(format!("{base}/")).send().await.expect("GET /");
    let id2 = resp.headers()["x-request-id"].to_str().expect("id header");
    assert_ne!(id1, id2);

    // Protection responses carry an id too.
    let resp = client
        .get(format!("{base}/?q={}", "x".repeat(200)))
        .send()
        .await
        .expect("long uri");
    assert_eq!(resp.status(), 414);
    assert!(resp.headers().contains_key("x-request-id"));

    // Declared body above the limit → 413, before Lua runs.
    let resp = client
        .post(format!("{base}/upload"))
        .body(vec![0u8; 4096])
        .send()
        .await
        .expect("big upload");
    assert_eq!(resp.status(), 413);

    // At/below the limit passes.
    let resp = client
        .post(format!("{base}/upload"))
        .body(vec![0u8; 512])
        .send()
        .await
        .expect("small upload");
    assert_eq!(resp.status(), 200);

    let _ = stop_tx.send(());
    served.await.expect("task").expect("shutdown");
    std::fs::remove_file(&handler).ok();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn trusted_request_ids_pass_through() {
    let handler = write_temp_script("trusted.lua", APP_SCRIPT);

    let cfg = nitr::Config {
        trust_request_id: true,
        ..Default::default()
    };
    let (addr, stop_tx, served) = start(cfg, &handler).await;
    let client = reqwest::Client::new();

    let resp = client
        .get(format!("http://{addr}/"))
        .header("x-request-id", "req-from-proxy-1")
        .send()
        .await
        .expect("GET /");
    assert_eq!(resp.headers()["x-request-id"], "req-from-proxy-1");

    // Malformed inbound ids are replaced, not echoed.
    let resp = client
        .get(format!("http://{addr}/"))
        .header("x-request-id", "bad id with spaces")
        .send()
        .await
        .expect("GET /");
    assert_ne!(resp.headers()["x-request-id"], "bad id with spaces");

    let _ = stop_tx.send(());
    served.await.expect("task").expect("shutdown");
    std::fs::remove_file(&handler).ok();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rate_limiting_answers_429_with_retry_after() {
    let handler = write_temp_script("limited.lua", APP_SCRIPT);

    let mut cfg = nitr::Config::default();
    cfg.rate_limit.enabled = true;
    cfg.rate_limit.requests = 3;
    cfg.rate_limit.window = 60;

    let (addr, stop_tx, served) = start(cfg, &handler).await;
    let base = format!("http://{addr}");
    let client = reqwest::Client::new();

    for i in 1..=3 {
        let resp = client
            .get(format!("{base}/"))
            .send()
            .await
            .expect("within budget");
        assert_eq!(resp.status(), 200, "request {i} should pass");
    }
    let resp = client
        .get(format!("{base}/"))
        .send()
        .await
        .expect("over budget");
    assert_eq!(resp.status(), 429);
    let retry: u64 = resp.headers()["retry-after"]
        .to_str()
        .expect("retry-after")
        .parse()
        .expect("retry-after seconds");
    assert!((1..=60).contains(&retry));

    let _ = stop_tx.send(());
    served.await.expect("task").expect("shutdown");
    std::fs::remove_file(&handler).ok();
}
