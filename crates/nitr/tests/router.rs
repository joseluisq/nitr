//! End-to-end tests for the `nitr.app()` router/middleware model: Rust-side
//! matching (404/405), path parameters, middleware composition and
//! short-circuiting, the app error handler, and `nitr.cfg`.

use std::net::SocketAddr;
use std::path::PathBuf;

const APP_SCRIPT: &str = r#"
local app = nitr.app()

-- Global middleware: tags every routed response.
app:use(function(next)
    return function(req)
        local res = next(req)
        res.headers = res.headers or {}
        res.headers["X-Global"] = "1"
        return res
    end
end)

local function auth(next)
    return function(req)
        if req.headers["authorization"] ~= "secret" then
            return { status = 401, body = "Unauthorized" }
        end
        return next(req)
    end
end

app:get("/", function(req)
    return { status = 200, body = "home" }
end)

app:get("/users/:id", function(req)
    return { status = 200, body = "user " .. req.params.id }
end)

app:post("/users", function(req)
    return { status = 201, body = "created" }
end)

app:get("/admin", auth, function(req)
    return { status = 200, body = "admin" }
end)

app:get("/files/*", function(req)
    return { status = 200, body = req.params.splat }
end)

app:get("/boom", function(req)
    error("kaboom")
end)

app:get("/cfg", function(req)
    return { status = 200, body = nitr.cfg and nitr.cfg.name or "no cfg" }
end)

app:on_error(function(err, req)
    return {
        status = 500,
        headers = { ["X-Err"] = "handled" },
        body = "handled: " .. req.path,
    }
end)

return app
"#;

const CFG_SCRIPT: &str = r#"
function(db)
    return { name = "from-config" }
end
"#;

fn write_temp_script(name: &str, content: &str) -> PathBuf {
    let path = std::env::temp_dir().join(format!("nitr-router-{}-{name}", std::process::id()));
    std::fs::write(&path, content).expect("write temp script");
    path
}

/// Grabs a free loopback port from the OS.
fn free_addr() -> SocketAddr {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind port 0");
    listener.local_addr().expect("local addr")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn routes_middleware_and_errors_end_to_end() {
    let handler = write_temp_script("app.lua", APP_SCRIPT);
    let config = write_temp_script("cfg.lua", CFG_SCRIPT);
    let addr = free_addr();

    let server = nitr::Server::builder()
        .listen(addr)
        .handler_script(&handler)
        .config_script(&config)
        .builtins(nitr::Builtins::JSON)
        .workers(2)
        .build()
        .await
        .expect("build server");

    let (stop_tx, stop_rx) = tokio::sync::oneshot::channel::<()>();
    let served = tokio::spawn(server.serve_with_shutdown(async {
        let _ = stop_rx.await;
    }));

    let base = format!("http://{addr}");
    let client = reqwest::Client::new();

    // Plain route + global middleware tag.
    let resp = client.get(format!("{base}/")).send().await.expect("GET /");
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.headers()["x-global"], "1");
    assert_eq!(resp.text().await.expect("body"), "home");

    // Path parameters.
    let resp = client
        .get(format!("{base}/users/42"))
        .send()
        .await
        .expect("GET /users/42");
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.text().await.expect("body"), "user 42");

    // Method routing.
    let resp = client
        .post(format!("{base}/users"))
        .send()
        .await
        .expect("POST /users");
    assert_eq!(resp.status(), 201);

    // 405 with an Allow header for a known path with the wrong method.
    let resp = client
        .delete(format!("{base}/users/42"))
        .send()
        .await
        .expect("DELETE /users/42");
    assert_eq!(resp.status(), 405);
    assert_eq!(resp.headers()["allow"], "GET");

    // 404 for an unregistered path: Lua is never invoked.
    let resp = client
        .get(format!("{base}/nope"))
        .send()
        .await
        .expect("GET /nope");
    assert_eq!(resp.status(), 404);

    // Per-route middleware short-circuits without auth...
    let resp = client
        .get(format!("{base}/admin"))
        .send()
        .await
        .expect("GET /admin");
    assert_eq!(resp.status(), 401);
    // ...but the global middleware still wrapped the response.
    assert_eq!(resp.headers()["x-global"], "1");

    // ...and passes through with credentials.
    let resp = client
        .get(format!("{base}/admin"))
        .header("authorization", "secret")
        .send()
        .await
        .expect("GET /admin authorized");
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.text().await.expect("body"), "admin");

    // Trailing catch-all captures the rest of the path.
    let resp = client
        .get(format!("{base}/files/a/b.txt"))
        .send()
        .await
        .expect("GET /files/a/b.txt");
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.text().await.expect("body"), "a/b.txt");

    // A handler error reaches app:on_error with the same request object.
    let resp = client
        .get(format!("{base}/boom"))
        .send()
        .await
        .expect("GET /boom");
    assert_eq!(resp.status(), 500);
    assert_eq!(resp.headers()["x-err"], "handled");
    assert_eq!(resp.text().await.expect("body"), "handled: /boom");

    // The config snapshot is reachable as nitr.cfg.
    let resp = client
        .get(format!("{base}/cfg"))
        .send()
        .await
        .expect("GET /cfg");
    assert_eq!(resp.text().await.expect("body"), "from-config");

    let _ = stop_tx.send(());
    served
        .await
        .expect("server task")
        .expect("server shutdown cleanly");

    std::fs::remove_file(&handler).ok();
    std::fs::remove_file(&config).ok();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn use_after_a_route_fails_at_startup() {
    let handler = write_temp_script(
        "use-after-route.lua",
        r#"
        local app = nitr.app()
        app:get("/", function(req) return { status = 200 } end)
        app:use(function(next) return next end)
        return app
        "#,
    );

    let err = nitr::Server::builder()
        .listen(free_addr())
        .handler_script(&handler)
        .builtins(nitr::Builtins::JSON)
        .workers(1)
        .build()
        .await
        .expect_err("app:use after a route must fail the build");
    assert!(
        err.to_string().contains("before registering routes"),
        "got: {err}"
    );

    std::fs::remove_file(&handler).ok();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn duplicate_routes_fail_at_startup() {
    let handler = write_temp_script(
        "duplicate.lua",
        r#"
        local app = nitr.app()
        app:get("/x", function(req) return { status = 200 } end)
        app:get("/x", function(req) return { status = 200 } end)
        return app
        "#,
    );

    let err = nitr::Server::builder()
        .listen(free_addr())
        .handler_script(&handler)
        .builtins(nitr::Builtins::JSON)
        .workers(1)
        .build()
        .await
        .expect_err("duplicate route must fail the build");
    assert!(err.to_string().contains("duplicate route"), "got: {err}");

    std::fs::remove_file(&handler).ok();
}
