//! End-to-end tests for phase 10: what Nitr does when things go wrong.
//!
//! Load shedding, panic containment, the sandbox budget applied to
//! user-created coroutines, request-body counting, client disconnects, and
//! the graceful-shutdown drain — each asserted through the real server.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;

use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

const APP_SCRIPT: &str = r#"
local app = nitr.app()

app:get("/ok", function(req)
    return nitr.text("ok")
end)

-- Suspends without burning instructions, so the state stays checked out.
app:get("/slow", function(req)
    nitr.testutil.sleep(3000)
    return nitr.text("slow")
end)

-- A panic raised in Rust, on the far side of the extension boundary.
app:get("/panic", function(req)
    nitr.testutil.boom()
    return nitr.text("unreachable")
end)

-- The sandbox escape phase 10 closes: a hot loop inside a coroutine the
-- script created itself, which a per-thread hook would never see.
app:get("/coroutine-spin", function(req)
    local spin = coroutine.wrap(function()
        while true do end
    end)
    spin()
    return nitr.text("unreachable")
end)

app:post("/echo", function(req)
    return nitr.text(req:text())
end)

return app
"#;

fn write_temp_script(name: &str, content: &str) -> PathBuf {
    let path = std::env::temp_dir().join(format!("nitr-resilience-{}-{name}", std::process::id()));
    std::fs::write(&path, content).expect("write temp script");
    path
}

fn free_addr() -> SocketAddr {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind port 0");
    listener.local_addr().expect("local addr")
}

/// Waits for the spawned server to actually bind, so a test that opens a
/// raw socket does not race the `serve()` task to the listener.
async fn wait_until_listening(addr: SocketAddr) {
    for _ in 0..200 {
        if tokio::net::TcpStream::connect(addr).await.is_ok() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("the server never started listening on {addr}");
}

/// A Lua-visible module that can do the two things no sandboxed script can:
/// suspend on a real timer, and panic in Rust.
fn testutil(lua: &mlua::Lua) -> mlua::Result<mlua::Table> {
    let t = lua.create_table()?;
    t.set(
        "sleep",
        lua.create_async_function(|_, ms: u64| async move {
            tokio::time::sleep(Duration::from_millis(ms)).await;
            Ok(())
        })?,
    )?;
    t.set(
        "boom",
        lua.create_function(|_, ()| -> mlua::Result<()> {
            panic!("boom from a Rust extension module")
        })?,
    )?;
    Ok(t)
}

/// Builds a one-state server on a free port with the shared app script.
async fn build(cfg: nitr::Config) -> (nitr::Server, SocketAddr, PathBuf) {
    let handler = write_temp_script("app.lua", APP_SCRIPT);
    let addr = cfg.listen;
    let server = nitr::Server::builder()
        .config(cfg)
        .handler_script(&handler)
        .builtins(nitr::Builtins::JSON | nitr::Builtins::HTTP)
        .module("testutil", testutil)
        .build()
        .await
        .expect("build server");
    (server, addr, handler)
}

fn base_config(workers: usize) -> nitr::Config {
    let mut cfg = nitr::Config {
        listen: free_addr(),
        workers,
        ..Default::default()
    };
    // Keep the drain short so shutdown assertions do not idle for 35s.
    cfg.shutdown.grace = 1;
    cfg.shutdown.stream_grace = 0;
    cfg
}

// ---------------------------------------------------------------------------

/// A single state, one slow request holding it: the next request must be shed
/// with 503 + `Retry-After` instead of queueing behind the pool.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_saturated_pool_sheds_instead_of_queueing() {
    let mut cfg = base_config(1);
    cfg.limits.pool_wait_ms = 200;
    let (server, addr, handler) = build(cfg).await;

    let (stop_tx, stop_rx) = tokio::sync::oneshot::channel::<()>();
    let served = tokio::spawn(server.serve_with_shutdown(async {
        let _ = stop_rx.await;
    }));
    wait_until_listening(addr).await;

    let base = format!("http://{addr}");
    let client = reqwest::Client::new();

    // Occupy the only state for 3s.
    let slow = tokio::spawn({
        let client = client.clone();
        let url = format!("{base}/slow");
        async move { client.get(url).send().await }
    });
    // Give the slow request time to check the state out.
    tokio::time::sleep(Duration::from_millis(300)).await;

    let resp = client
        .get(format!("{base}/ok"))
        .send()
        .await
        .expect("shed response");
    assert_eq!(resp.status(), 503);
    assert_eq!(resp.headers()["retry-after"], "1");

    // The slow request itself is unaffected.
    let slow = slow.await.expect("slow task").expect("slow response");
    assert_eq!(slow.status(), 200);
    assert_eq!(slow.text().await.expect("slow body"), "slow");

    // And the state is back in circulation afterwards.
    let resp = client
        .get(format!("{base}/ok"))
        .send()
        .await
        .expect("recovered response");
    assert_eq!(resp.status(), 200);

    let _ = stop_tx.send(());
    served.await.expect("server task").expect("clean shutdown");
    std::fs::remove_file(&handler).ok();
}

/// A panic in Rust code called from Lua becomes a 500 instead of killing the
/// connection, and the damaged state is recycled so the pool keeps its size.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_panic_becomes_a_500_and_recycles_the_state() {
    let (server, addr, handler) = build(base_config(1)).await;
    let pool = server.pool();

    let (stop_tx, stop_rx) = tokio::sync::oneshot::channel::<()>();
    let served = tokio::spawn(server.serve_with_shutdown(async {
        let _ = stop_rx.await;
    }));
    wait_until_listening(addr).await;

    let base = format!("http://{addr}");
    let client = reqwest::Client::new();

    let resp = client
        .get(format!("{base}/panic"))
        .send()
        .await
        .expect("the connection must survive the panic");
    assert_eq!(resp.status(), 500);

    // The state was dropped and rebuilt off the request path; wait for the
    // replacement rather than assuming an ordering.
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while pool.available() == 0 && std::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(pool.size(), 1, "the pool must not shrink");

    // And the server keeps serving on the replacement.
    let resp = client
        .get(format!("{base}/ok"))
        .send()
        .await
        .expect("post-panic response");
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.text().await.expect("body"), "ok");

    let _ = stop_tx.send(());
    served.await.expect("server task").expect("clean shutdown");
    std::fs::remove_file(&handler).ok();
}

/// The execution budget reaches inside a coroutine the script created, which
/// a per-thread hook would let run forever.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_user_coroutine_cannot_escape_the_execution_budget() {
    let mut cfg = base_config(1);
    cfg.lua.exec_timeout_ms = 1_000;
    let (server, addr, handler) = build(cfg).await;

    let (stop_tx, stop_rx) = tokio::sync::oneshot::channel::<()>();
    let served = tokio::spawn(server.serve_with_shutdown(async {
        let _ = stop_rx.await;
    }));
    wait_until_listening(addr).await;

    let base = format!("http://{addr}");
    let client = reqwest::Client::new();

    let started = std::time::Instant::now();
    let resp = tokio::time::timeout(
        Duration::from_secs(10),
        client.get(format!("{base}/coroutine-spin")).send(),
    )
    .await
    .expect("the spinning coroutine must be stopped, not run forever")
    .expect("response");
    assert_eq!(resp.status(), 500);
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "stopped after {:?}, well past the 1s budget",
        started.elapsed()
    );

    // The state recovers and serves the next request.
    let resp = client
        .get(format!("{base}/ok"))
        .send()
        .await
        .expect("post-timeout response");
    assert_eq!(resp.status(), 200);

    let _ = stop_tx.send(());
    served.await.expect("server task").expect("clean shutdown");
    std::fs::remove_file(&handler).ok();
}

/// A chunked body declares no length, so the ceiling has to be enforced on
/// the bytes that actually arrive.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_oversized_chunked_body_is_rejected_with_413() {
    let mut cfg = base_config(1);
    cfg.limits.max_body_bytes = 1024;
    let (server, addr, handler) = build(cfg).await;

    let (stop_tx, stop_rx) = tokio::sync::oneshot::channel::<()>();
    let served = tokio::spawn(server.serve_with_shutdown(async {
        let _ = stop_rx.await;
    }));
    wait_until_listening(addr).await;

    let base = format!("http://{addr}");
    let client = reqwest::Client::new();

    // A chunked body carries no Content-Length, so the declared-size check
    // cannot see it: only the running count can. Written on a raw socket
    // because the test client always sets a length.
    let mut sock = tokio::net::TcpStream::connect(addr).await.expect("connect");
    sock.write_all(b"POST /echo HTTP/1.1\r\nHost: localhost\r\nTransfer-Encoding: chunked\r\n\r\n")
        .await
        .expect("write headers");
    // 4 KiB in 512-byte chunks, four times the 1 KiB ceiling. The writes are
    // best-effort: the server is entitled to answer and hang up part-way
    // through, which is the whole point of counting as bytes arrive.
    for _ in 0..8 {
        if sock.write_all(b"200\r\n").await.is_err()
            || sock.write_all(&[b'x'; 512]).await.is_err()
            || sock.write_all(b"\r\n").await.is_err()
        {
            break;
        }
    }
    let _ = sock.write_all(b"0\r\n\r\n").await;

    // Read just the status line: the connection may stay open afterwards.
    let mut raw = [0u8; 128];
    let n = tokio::time::timeout(Duration::from_secs(5), sock.read(&mut raw))
        .await
        .expect("the server must answer, not hang")
        .expect("read response");
    let head = String::from_utf8_lossy(&raw[..n]);
    let status = head.lines().next().unwrap_or_default();
    assert!(
        status.contains("413"),
        "expected a 413 status line, got: {status}"
    );
    // Hang up: the unread tail of the body would otherwise keep this
    // connection busy through the drain below.
    drop(sock);

    // A body under the ceiling still round-trips.
    let resp = client
        .post(format!("{base}/echo"))
        .body("small")
        .send()
        .await
        .expect("small response");
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.text().await.expect("body"), "small");

    let _ = stop_tx.send(());
    served.await.expect("server task").expect("clean shutdown");
    std::fs::remove_file(&handler).ok();
}

/// When the client hangs up mid-request, the handler future is dropped and
/// the state it held goes back to the pool — it is not held for the full
/// handler duration serving a response nobody will read.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_client_disconnect_releases_the_pooled_state() {
    let mut cfg = base_config(1);
    cfg.limits.pool_wait_ms = 500;
    let (server, addr, handler) = build(cfg).await;

    let (stop_tx, stop_rx) = tokio::sync::oneshot::channel::<()>();
    let served = tokio::spawn(server.serve_with_shutdown(async {
        let _ = stop_rx.await;
    }));
    wait_until_listening(addr).await;

    // Raw socket: send the request, then hang up while the handler sleeps.
    let mut sock = tokio::net::TcpStream::connect(addr).await.expect("connect");
    sock.write_all(b"GET /slow HTTP/1.1\r\nHost: localhost\r\n\r\n")
        .await
        .expect("write request");
    tokio::time::sleep(Duration::from_millis(300)).await;
    drop(sock);
    // Let hyper notice the peer is gone and drop the handler future.
    tokio::time::sleep(Duration::from_millis(300)).await;

    // The 3s handler is long gone; if the state were still checked out this
    // would exceed the 500ms wait budget and come back 503.
    let client = reqwest::Client::new();
    let resp = client
        .get(format!("http://{addr}/ok"))
        .send()
        .await
        .expect("post-disconnect response");
    assert_eq!(
        resp.status(),
        200,
        "the abandoned request must not keep holding the only Lua state"
    );

    let _ = stop_tx.send(());
    served.await.expect("server task").expect("clean shutdown");
    std::fs::remove_file(&handler).ok();
}

/// An in-flight request finishes after the shutdown signal, and the server
/// reports a clean drain.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn shutdown_drains_in_flight_requests() {
    let mut cfg = base_config(2);
    // The slow handler needs 3s; give the drain room to finish it.
    cfg.shutdown.grace = 10;
    let (server, addr, handler) = build(cfg).await;

    let (stop_tx, stop_rx) = tokio::sync::oneshot::channel::<()>();
    let served = tokio::spawn(server.serve_with_shutdown(async {
        let _ = stop_rx.await;
    }));
    wait_until_listening(addr).await;

    let client = reqwest::Client::new();
    let inflight = tokio::spawn({
        let client = client.clone();
        let url = format!("http://{addr}/slow");
        async move { client.get(url).send().await }
    });
    tokio::time::sleep(Duration::from_millis(300)).await;

    // Signal shutdown while the request is still running.
    let _ = stop_tx.send(());

    let resp = inflight
        .await
        .expect("in-flight task")
        .expect("the in-flight request must complete, not be cut");
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.text().await.expect("body"), "slow");

    served
        .await
        .expect("server task")
        .expect("a drained shutdown is not an error");

    // The listener is closed: no new connection is accepted.
    assert!(
        tokio::net::TcpStream::connect(addr).await.is_err()
            || client
                .get(format!("http://{addr}/ok"))
                .send()
                .await
                .is_err(),
        "the server must stop accepting after the drain"
    );

    std::fs::remove_file(&handler).ok();
}

/// A request that outlives the drain deadline is cut, and the server says so
/// rather than exiting as if nothing happened.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_expired_drain_deadline_is_reported() {
    // 1s of grace against a 3s handler.
    let (server, addr, handler) = build(base_config(2)).await;

    let (stop_tx, stop_rx) = tokio::sync::oneshot::channel::<()>();
    let served = tokio::spawn(server.serve_with_shutdown(async {
        let _ = stop_rx.await;
    }));
    wait_until_listening(addr).await;

    let client = reqwest::Client::new();
    let inflight = tokio::spawn({
        let url = format!("http://{addr}/slow");
        async move { client.get(url).send().await }
    });
    tokio::time::sleep(Duration::from_millis(300)).await;
    let _ = stop_tx.send(());

    let err = served
        .await
        .expect("server task")
        .expect_err("a truncated shutdown must surface");
    assert!(matches!(err, nitr::Error::ShutdownTimeout), "got: {err:?}");

    // The abandoned request was cut rather than answered.
    let _ = inflight.await;
    std::fs::remove_file(&handler).ok();
}
