//! End-to-end test: build a real server with the builder API, serve over a
//! TCP socket, exercise the Lua handler, and shut down gracefully.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};

fn write_temp_script(name: &str, content: &str) -> PathBuf {
    // `fs::write` truncates before writing, so a path two tests share is a
    // race; the counter keeps every call on its own file.
    static NEXT: AtomicU32 = AtomicU32::new(0);
    let id = NEXT.fetch_add(1, Ordering::Relaxed);
    let path = std::env::temp_dir().join(format!("nitr-it-{}-{id}-{name}", std::process::id()));
    std::fs::write(&path, content).expect("write temp script");
    path
}

/// Grabs a free loopback port from the OS.
/// Binds port 0 (the OS picks a free port) and keeps the listener alive.
/// The server adopts it via `.listener(...)`, so the port can never be
/// taken by another test between choosing it and serving on it.
fn reserve_addr() -> (std::net::TcpListener, SocketAddr) {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind port 0");
    let addr = listener.local_addr().expect("local addr");
    (listener, addr)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn serves_lua_handlers_end_to_end() {
    let handler = write_temp_script(
        "handler.lua",
        r#"
        local app = nitr.app()

        app:get("/binary", function(req)
            return {
                status = 200,
                headers = { ["Set-Cookie"] = { "a=1", "b=2" } },
                body = string.char(0, 255) .. "end",
            }
        end)

        app:get("/boom", function(req)
            error("kaboom")
        end)

        app:get("/hello", function(req)
            return {
                status = 200,
                headers = { ["Content-Type"] = "application/json" },
                body = nitr.json:encode({
                    path = req.path,
                    name = req.query.name,
                    greeting = greet(req.query.name or "world"),
                    from_cfg = nitr.cfg.motto,
                }),
            }
        end)

        return app
        "#,
    );
    let config = write_temp_script(
        "config.lua",
        "function() return { motto = 'fast and safe' } end",
    );

    let (listener, addr) = reserve_addr();
    let server = nitr::Server::builder()
        .listen(addr)
        .listener(listener)
        .handler_script(&handler)
        .config_script(&config)
        .builtins(nitr::Builtins::JSON)
        .workers(2)
        .setup(|lua| {
            let greet = lua.create_function(|_, name: String| Ok(format!("Hello, {name}!")))?;
            lua.globals().set("greet", greet)
        })
        .build()
        .await
        .expect("build server");

    let (stop_tx, stop_rx) = tokio::sync::oneshot::channel::<()>();
    let task = tokio::spawn(server.serve_with_shutdown(async {
        let _ = stop_rx.await;
    }));

    // Wait until the server accepts connections.
    let client = reqwest::Client::new();
    let base = format!("http://{addr}");
    let mut ready = false;
    for _ in 0..50 {
        if client.get(&base).send().await.is_ok() {
            ready = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    assert!(ready, "server did not become ready");

    // JSON route: query parsing, custom setup() global, config snapshot.
    let resp = client
        .get(format!("{base}/hello?name=Jos%C3%A9"))
        .send()
        .await
        .expect("json request");
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.headers()["content-type"], "application/json");
    let json: serde_json::Value = resp.json().await.expect("json body");
    assert_eq!(json["path"], "/hello");
    assert_eq!(json["name"], "José");
    assert_eq!(json["greeting"], "Hello, José!");
    assert_eq!(json["from_cfg"], "fast and safe");

    // Binary body and multi-value headers survive.
    let resp = client
        .get(format!("{base}/binary"))
        .send()
        .await
        .expect("binary request");
    let cookies: Vec<_> = resp.headers().get_all("set-cookie").iter().collect();
    assert_eq!(cookies, ["a=1", "b=2"]);
    let body = resp.bytes().await.expect("binary body");
    assert_eq!(&body[..], &[0, 255, b'e', b'n', b'd']);

    // Script errors become a generic 500 without leaking details.
    let resp = client
        .get(format!("{base}/boom"))
        .send()
        .await
        .expect("error request");
    assert_eq!(resp.status(), 500);
    let body = resp.text().await.expect("error body");
    assert_eq!(body, "Internal Server Error");

    // Graceful shutdown.
    stop_tx.send(()).expect("send shutdown");
    task.await
        .expect("join server task")
        .expect("server exits cleanly");

    std::fs::remove_file(&handler).ok();
    std::fs::remove_file(&config).ok();
}
