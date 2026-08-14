//! End-to-end tests for the phase-7 platform features: static file
//! serving (conditional requests, traversal protection, SPA fallback,
//! `app:static` and `[static]` config), and the in-process test client.

use std::net::SocketAddr;
use std::path::PathBuf;

fn scratch_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("nitr-platform-{}-{name}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("mkdir");
    dir
}

fn free_addr() -> SocketAddr {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind port 0");
    listener.local_addr().expect("local addr")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn static_files_and_spa_end_to_end() {
    // Site layout: index.html + assets/app.js, plus an SPA mount.
    let site = scratch_dir("site");
    std::fs::create_dir_all(site.join("assets")).expect("mkdir assets");
    std::fs::write(site.join("index.html"), "<h1>home</h1>").expect("write index");
    std::fs::write(site.join("assets/app.js"), "console.log('hi')").expect("write js");

    let spa = scratch_dir("spa");
    std::fs::write(spa.join("index.html"), "<div id=app></div>").expect("write spa index");

    let app = format!(
        r#"
local app = nitr.app()
app:static("/", "{site}")
app:static("/spa", "{spa}", {{ spa = true }})
app:get("/api/ping", function(req) return json({{ pong = true }}) end)
return app
"#,
        site = site.display(),
        spa = spa.display(),
    );
    let handler = std::env::temp_dir().join(format!("nitr-platform-{}.lua", std::process::id()));
    std::fs::write(&handler, app).expect("write handler");

    let addr = free_addr();
    let server = nitr::Server::builder()
        .listen(addr)
        .handler_script(&handler)
        .builtins(nitr::Builtins::JSON | nitr::Builtins::HTTP)
        .workers(1)
        .build()
        .await
        .expect("build server");

    let (stop_tx, stop_rx) = tokio::sync::oneshot::channel::<()>();
    let served = tokio::spawn(server.serve_with_shutdown(async {
        let _ = stop_rx.await;
    }));
    let base = format!("http://{addr}");
    let client = reqwest::Client::new();

    // Directory index + content type.
    let resp = client.get(format!("{base}/")).send().await.expect("index");
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.headers()["content-type"], "text/html");
    let etag = resp.headers()["etag"].to_str().expect("etag").to_string();
    assert!(resp.headers().contains_key("last-modified"));
    assert_eq!(resp.text().await.expect("body"), "<h1>home</h1>");

    // Conditional revalidation with the returned ETag.
    let resp = client
        .get(format!("{base}/"))
        .header("if-none-match", &etag)
        .send()
        .await
        .expect("conditional");
    assert_eq!(resp.status(), 304);
    assert!(resp.text().await.expect("empty body").is_empty());

    // Nested asset with a JS content type.
    let resp = client
        .get(format!("{base}/assets/app.js"))
        .send()
        .await
        .expect("asset");
    assert_eq!(resp.status(), 200);
    assert!(resp.headers()["content-type"]
        .to_str()
        .expect("ct")
        .contains("javascript"));

    // Traversal attempts never leave the mount.
    for path in [
        "/../Cargo.toml",
        "/%2e%2e/Cargo.toml",
        "/assets/../../etc/passwd",
    ] {
        let resp = client
            .get(format!("{base}{path}"))
            .send()
            .await
            .expect("traversal");
        assert_eq!(resp.status(), 404, "{path} must be rejected");
    }

    // Lua routes still win over the root static mount.
    let resp = client
        .get(format!("{base}/api/ping"))
        .send()
        .await
        .expect("api");
    assert_eq!(resp.headers()["content-type"], "application/json");

    // SPA fallback serves the index for unknown paths under its mount.
    let resp = client
        .get(format!("{base}/spa/some/client/route"))
        .send()
        .await
        .expect("spa");
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.text().await.expect("spa body"), "<div id=app></div>");

    // Unknown path outside any mount is still a 404.
    let resp = client
        .get(format!("{base}/missing.txt"))
        .send()
        .await
        .expect("missing");
    assert_eq!(resp.status(), 404);

    let _ = stop_tx.send(());
    served.await.expect("task").expect("shutdown");
    std::fs::remove_file(&handler).ok();
    std::fs::remove_dir_all(&site).ok();
    std::fs::remove_dir_all(&spa).ok();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_client_dispatches_in_process() {
    let handler = std::env::temp_dir().join(format!("nitr-tc-{}.lua", std::process::id()));
    std::fs::write(
        &handler,
        r#"
        local app = nitr.app()
        app:get("/hello/:name", function(req)
            return json({ hello = req.params.name, ua = req.headers["user-agent"] })
        end)
        app:post("/echo", function(req)
            return text(req:text(), 201)
        end)
        return app
        "#,
    )
    .expect("write handler");

    // No listen address is ever bound: the client dispatches in-process.
    let server = nitr::Server::builder()
        .handler_script(&handler)
        .builtins(nitr::Builtins::JSON | nitr::Builtins::HTTP)
        .workers(1)
        .build()
        .await
        .expect("build server");
    let client = server.test_client();

    let resp = client
        .request(
            "get",
            "/hello/nitr",
            &[("user-agent".into(), "nitr-test".into())],
            None,
        )
        .await
        .expect("request");
    assert_eq!(resp.status, 200);
    assert_eq!(resp.header("content-type"), Some("application/json"));
    assert!(resp.header("x-request-id").is_some());
    let body: serde_json::Value = serde_json::from_slice(&resp.body).expect("json");
    assert_eq!(body["hello"], "nitr");
    assert_eq!(body["ua"], "nitr-test");

    let resp = client
        .request("POST", "/echo", &[], Some("payload".into()))
        .await
        .expect("post");
    assert_eq!(resp.status, 201);
    assert_eq!(&resp.body[..], b"payload");

    // Router misses stay router misses in-process.
    let resp = client
        .request("GET", "/nope", &[], None)
        .await
        .expect("404");
    assert_eq!(resp.status, 404);

    std::fs::remove_file(&handler).ok();
}
