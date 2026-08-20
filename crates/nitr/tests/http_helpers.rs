//! End-to-end tests for the phase-3 HTTP ergonomics: response helpers,
//! `nitr.error`, plain and signed cookies, and content negotiation.
//! (Named `http_helpers` because `harness/` is the shared test harness;
//! this file is a *test of* the response-helper API, not shared helpers.)

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};

const APP_SCRIPT: &str = r#"
local SECRET = "s3cret"
local app = nitr.app()

app:get("/text", function(req)
    return nitr.text("plain body")
end)

app:get("/html", function(req)
    return nitr.html("<p>hi</p>", 202)
end)

app:get("/json", function(req)
    return nitr.json({ n = 7 })
end)

app:get("/redirect", function(req)
    return nitr.redirect("/text")
end)

app:get("/nocontent", function(req)
    return nitr.status(204)
end)

app:get("/teapot", function(req)
    return nitr.error(418, { code = "TEAPOT" })
end)

app:get("/set", function(req)
    local res = nitr.text("ok")
    res.cookies:set("plain", "v1", { path = "/", http_only = true })
    res.cookies:set("extra", "v2")
    res.cookies:set_signed("session", "user-42", SECRET, { same_site = "Strict" })
    return res
end)

app:get("/read", function(req)
    return nitr.json({
        plain = req.cookies.plain,
        verified = req.cookies:verify("session", SECRET),
        forged = req.cookies:verify("plain", SECRET),
    })
end)

app:get("/negotiate", function(req)
    return nitr.negotiate(req, {
        ["application/json"] = function(r) return nitr.json({ kind = "json" }) end,
        ["text/html"] = function(r) return nitr.html("<p>html</p>") end,
    })
end)

app:get("/accepts", function(req)
    return nitr.text(req:accepts("text/html", "application/json") or "none")
end)

return app
"#;

fn write_temp_script(name: &str, content: &str) -> PathBuf {
    // `fs::write` truncates before writing, so a path two tests share is a
    // race; the counter keeps every call on its own file.
    static NEXT: AtomicU32 = AtomicU32::new(0);
    let id = NEXT.fetch_add(1, Ordering::Relaxed);
    let path =
        std::env::temp_dir().join(format!("nitr-helpers-{}-{id}-{name}", std::process::id()));
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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn helpers_cookies_and_negotiation_end_to_end() {
    let handler = write_temp_script("app.lua", APP_SCRIPT);
    let (listener, addr) = reserve_addr();

    let server = nitr::Server::builder()
        .listen(addr)
        .listener(listener)
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
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .expect("client");

    // nitr.text() / nitr.html() with optional status / callable nitr.json() helper.
    let resp = client
        .get(format!("{base}/text"))
        .send()
        .await
        .expect("text");
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.headers()["content-type"], "text/plain; charset=utf-8");
    assert_eq!(resp.text().await.expect("body"), "plain body");

    let resp = client
        .get(format!("{base}/html"))
        .send()
        .await
        .expect("html");
    assert_eq!(resp.status(), 202);
    assert_eq!(resp.headers()["content-type"], "text/html; charset=utf-8");

    let resp = client
        .get(format!("{base}/json"))
        .send()
        .await
        .expect("json");
    assert_eq!(resp.headers()["content-type"], "application/json");
    let body: serde_json::Value = resp.json().await.expect("json body");
    assert_eq!(body["n"], 7);

    // nitr.redirect() and nitr.status().
    let resp = client
        .get(format!("{base}/redirect"))
        .send()
        .await
        .expect("redirect");
    assert_eq!(resp.status(), 302);
    assert_eq!(resp.headers()["location"], "/text");

    let resp = client
        .get(format!("{base}/nocontent"))
        .send()
        .await
        .expect("status(204)");
    assert_eq!(resp.status(), 204);

    // nitr.error with a JSON body.
    let resp = client
        .get(format!("{base}/teapot"))
        .send()
        .await
        .expect("teapot");
    assert_eq!(resp.status(), 418);
    assert_eq!(resp.headers()["content-type"], "application/json");
    let body: serde_json::Value = resp.json().await.expect("error body");
    assert_eq!(body["code"], "TEAPOT");

    // Cookies: three Set-Cookie headers with their attributes.
    let resp = client.get(format!("{base}/set")).send().await.expect("set");
    let cookies: Vec<String> = resp
        .headers()
        .get_all("set-cookie")
        .iter()
        .map(|v| v.to_str().expect("cookie header").to_string())
        .collect();
    assert_eq!(cookies.len(), 3);
    assert!(cookies[0].contains("plain=v1") && cookies[0].contains("HttpOnly"));
    assert!(cookies[1].starts_with("extra=v2"));
    assert!(cookies[2].starts_with("session=") && cookies[2].contains("SameSite=Strict"));
    let session_value = cookies[2]
        .split(';')
        .next()
        .expect("cookie pair")
        .trim_start_matches("session=")
        .to_string();

    // Read them back: plain value, verified signed value, and a forged
    // verify (wrong cookie) yielding nil.
    let resp = client
        .get(format!("{base}/read"))
        .header("cookie", format!("plain=v1; session={session_value}"))
        .send()
        .await
        .expect("read");
    let body: serde_json::Value = resp.json().await.expect("read body");
    assert_eq!(body["plain"], "v1");
    assert_eq!(body["verified"], "user-42");
    assert!(body["forged"].is_null());

    // A tampered signed cookie fails verification.
    let resp = client
        .get(format!("{base}/read"))
        .header("cookie", format!("session=x{session_value}"))
        .send()
        .await
        .expect("tampered read");
    let body: serde_json::Value = resp.json().await.expect("tampered body");
    assert!(body["verified"].is_null());

    // nitr.negotiate() picks by Accept header.
    let resp = client
        .get(format!("{base}/negotiate"))
        .header("accept", "text/html")
        .send()
        .await
        .expect("negotiate html");
    assert_eq!(resp.headers()["content-type"], "text/html; charset=utf-8");

    let resp = client
        .get(format!("{base}/negotiate"))
        .header("accept", "application/json;q=0.9, text/html;q=0.1")
        .send()
        .await
        .expect("negotiate json");
    assert_eq!(resp.headers()["content-type"], "application/json");

    let resp = client
        .get(format!("{base}/negotiate"))
        .header("accept", "image/png")
        .send()
        .await
        .expect("negotiate none");
    assert_eq!(resp.status(), 406);

    // req:accepts returns the best of the offered types.
    let resp = client
        .get(format!("{base}/accepts"))
        .header("accept", "application/json")
        .send()
        .await
        .expect("accepts");
    assert_eq!(resp.text().await.expect("body"), "application/json");

    let _ = stop_tx.send(());
    served
        .await
        .expect("server task")
        .expect("server shutdown cleanly");

    std::fs::remove_file(&handler).ok();
}
