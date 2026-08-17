//! End-to-end tests for phase 15: health/readiness endpoints answered in
//! Rust, on the main listener or a separate bind.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};

const APP: &str = r#"
local app = nitr.app()
app:get("/hello", function(req)
    return nitr.json({ ok = true })
end)
return app
"#;

fn write_temp_script(name: &str) -> PathBuf {
    static NEXT: AtomicU32 = AtomicU32::new(0);
    let id = NEXT.fetch_add(1, Ordering::Relaxed);
    let path = std::env::temp_dir().join(format!("nitr-ops-{}-{id}-{name}", std::process::id()));
    std::fs::write(&path, APP).expect("write temp script");
    path
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
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    panic!("server never started listening on {addr}");
}

struct Harness {
    addr: SocketAddr,
    handler: PathBuf,
    stop: tokio::sync::oneshot::Sender<()>,
    served: tokio::task::JoinHandle<nitr::Result>,
}

impl Harness {
    async fn start(tune: impl FnOnce(&mut nitr::Config)) -> Self {
        let handler = write_temp_script("app.lua");
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
            .build()
            .await
            .expect("build server");
        let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
        let served = tokio::spawn(server.serve_with_shutdown(async move {
            let _ = stopped.await;
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
        let _ = self.served.await;
        std::fs::remove_file(&self.handler).ok();
    }
}

/// The default: probes answer on the main listener, in Rust, and the
/// application's own routes are untouched.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn health_endpoints_answer_on_the_main_listener() {
    let h = Harness::start(|_| {}).await;
    let client = reqwest::Client::new();

    let resp = client
        .get(h.url("/healthz"))
        .send()
        .await
        .expect("liveness");
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.headers()["cache-control"], "no-store");
    assert_eq!(resp.text().await.expect("body"), "ok");

    let resp = client
        .get(h.url("/readyz"))
        .send()
        .await
        .expect("readiness");
    assert_eq!(resp.status(), 200);

    // The application still owns everything else — including a POST to
    // the probe path, which is not a probe.
    let resp = client.get(h.url("/hello")).send().await.expect("app route");
    assert_eq!(resp.status(), 200);
    let resp = client.post(h.url("/healthz")).send().await.expect("post");
    assert_eq!(resp.status(), 404);

    h.stop().await;
}

/// `[health] enabled = false` removes the endpoints entirely.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn disabled_health_passes_through_to_the_app() {
    let h = Harness::start(|cfg| cfg.health.enabled = false).await;
    let resp = reqwest::get(h.url("/healthz")).await.expect("get");
    assert_eq!(resp.status(), 404);
    h.stop().await;
}

/// `[health] bind` moves the probes to their own address: the public port
/// no longer answers them, and the probe port answers nothing else.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn separate_bind_keeps_probes_off_the_public_port() {
    // Reserve a port for the probes, then release it for the server to
    // bind (a small race, but the port was just allocated to us).
    let probe_addr = {
        let (listener, addr) = reserve_addr();
        drop(listener);
        addr
    };
    let h = Harness::start(|cfg| cfg.health.bind = Some(probe_addr)).await;
    wait_until_listening(probe_addr).await;
    let client = reqwest::Client::new();

    // Probes on the probe port; the app never answers there.
    let probe = format!("http://{probe_addr}");
    let resp = client
        .get(format!("{probe}/healthz"))
        .send()
        .await
        .expect("liveness");
    assert_eq!(resp.status(), 200);
    let resp = client
        .get(format!("{probe}/readyz"))
        .send()
        .await
        .expect("readiness");
    assert_eq!(resp.status(), 200);
    let resp = client
        .get(format!("{probe}/hello"))
        .send()
        .await
        .expect("app path");
    assert_eq!(resp.status(), 404);

    // And the public port no longer serves the probes.
    let resp = client.get(h.url("/healthz")).send().await.expect("main");
    assert_eq!(resp.status(), 404);
    let resp = client.get(h.url("/hello")).send().await.expect("app");
    assert_eq!(resp.status(), 200);

    h.stop().await;
}
