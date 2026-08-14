//! End-to-end tests for phase 6: `await_all` concurrency, fetch options,
//! SSRF policy (default-deny, allow-list), policy-checked redirects, and
//! SQLite transactions (commit, rollback, savepoint nesting).

use std::net::SocketAddr;
use std::path::PathBuf;

fn write_temp(name: &str, content: &str) -> PathBuf {
    let path = std::env::temp_dir().join(format!("nitr-p6-{}-{name}", std::process::id()));
    std::fs::write(&path, content).expect("write temp file");
    path
}

fn free_addr() -> SocketAddr {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind port 0");
    listener.local_addr().expect("local addr")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn await_all_fetch_options_redirects_and_transactions() {
    let addr = free_addr();
    let db_path = std::env::temp_dir().join(format!("nitr-p6-{}.db", std::process::id()));
    std::fs::remove_file(&db_path).ok();

    let app = format!(
        r#"
local BASE = "http://{addr}"
local app = nitr.app()

app:get("/api/a", function(req) return json({{ name = "a" }}) end)
app:get("/api/b", function(req) return json({{ name = "b" }}) end)

-- Concurrent aggregation of two local endpoints.
app:get("/combined", function(req)
    local ra, rb = await_all(
        fetch("GET", BASE .. "/api/a"),
        fetch("GET", BASE .. "/api/b")
    )
    return json({{ first = ra:json().name, second = rb:json().name }})
end)

-- Echo endpoint + a fetch using the options table.
app:post("/echo", function(req)
    return json({{
        x = req.query.x,
        ct = req.headers["content-type"],
        body = req:json(),
    }})
end)

app:get("/opts", function(req)
    local resp = fetch("POST", BASE .. "/echo", {{
        query = {{ x = "42" }},
        json = {{ n = 7 }},
        timeout = 5,
    }}):send()
    return json(resp:json())
end)

-- Redirects are followed by the client (up to 5 hops), re-checked per hop.
app:get("/redir", function(req) return redirect("/target") end)
app:get("/target", function(req) return text("landed") end)
app:get("/follow", function(req)
    local resp = fetch("GET", BASE .. "/redir"):send()
    return json({{ status = resp.status, body = resp:text() }})
end)

-- Transactions: commit, rollback on error, savepoint nesting.
app:get("/tx", function(req)
    conn:execute("DELETE FROM t")

    conn:transaction(function(tx)
        tx:execute("INSERT INTO t (v) VALUES (?)", {{ "committed" }})
    end)

    local ok = pcall(function()
        conn:transaction(function(tx)
            tx:execute("INSERT INTO t (v) VALUES (?)", {{ "rolled-back" }})
            error("boom")
        end)
    end)

    conn:transaction(function(tx)
        tx:execute("INSERT INTO t (v) VALUES (?)", {{ "outer" }})
        pcall(function()
            tx:transaction(function(tx2)
                tx2:execute("INSERT INTO t (v) VALUES (?)", {{ "inner" }})
                error("inner boom")
            end)
        end)
    end)

    local rows = conn:query("SELECT v FROM t ORDER BY v")
    local values = {{}}
    for i, row in ipairs(rows) do values[i] = row.v end
    return json({{ failed_tx_ok = ok, values = values }})
end)

return app
"#
    );
    let handler = write_temp("app.lua", &app);
    let config = write_temp(
        "cfg.lua",
        r#"
        function(db)
            db:execute("CREATE TABLE IF NOT EXISTS t (v TEXT)")
            return {}
        end
        "#,
    );

    let mut cfg = nitr::Config {
        workers: 3,
        ..Default::default()
    };
    // Local aggregation: this test talks to itself over loopback.
    cfg.fetch.allow_private_networks = true;

    let server = nitr::Server::builder()
        .config(cfg)
        .listen(addr)
        .handler_script(&handler)
        .config_script(&config)
        .database(&db_path)
        .build()
        .await
        .expect("build server");

    let (stop_tx, stop_rx) = tokio::sync::oneshot::channel::<()>();
    let served = tokio::spawn(server.serve_with_shutdown(async {
        let _ = stop_rx.await;
    }));

    let base = format!("http://{addr}");
    let client = reqwest::Client::new();

    // await_all preserves argument order.
    let body: serde_json::Value = client
        .get(format!("{base}/combined"))
        .send()
        .await
        .expect("combined")
        .json()
        .await
        .expect("combined body");
    assert_eq!(body["first"], "a");
    assert_eq!(body["second"], "b");

    // Options table: query params, JSON body + content type, timeout.
    let body: serde_json::Value = client
        .get(format!("{base}/opts"))
        .send()
        .await
        .expect("opts")
        .json()
        .await
        .expect("opts body");
    assert_eq!(body["x"], "42");
    assert_eq!(body["ct"], "application/json");
    assert_eq!(body["body"]["n"], 7);

    // Manual redirect following.
    let body: serde_json::Value = client
        .get(format!("{base}/follow"))
        .send()
        .await
        .expect("follow")
        .json()
        .await
        .expect("follow body");
    assert_eq!(body["status"], 200);
    assert_eq!(body["body"], "landed");

    // Transactions: committed + outer survive; rolled-back + inner don't.
    let body: serde_json::Value = client
        .get(format!("{base}/tx"))
        .send()
        .await
        .expect("tx")
        .json()
        .await
        .expect("tx body");
    assert_eq!(body["failed_tx_ok"], false);
    assert_eq!(
        body["values"],
        serde_json::json!(["committed", "outer"]),
        "got: {body}"
    );

    let _ = stop_tx.send(());
    served.await.expect("task").expect("shutdown");
    std::fs::remove_file(&handler).ok();
    std::fs::remove_file(&config).ok();
    std::fs::remove_file(&db_path).ok();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fetch_policy_blocks_private_and_unlisted_hosts() {
    const APP: &str = r#"
local app = nitr.app()

app:get("/try", function(req)
    local ok, err = pcall(function()
        return fetch("GET", req.query.url):send()
    end)
    return json({ ok = ok, err = ok and "" or tostring(err) })
end)

return app
"#;

    // Default policy: private/loopback addresses are refused.
    let handler = write_temp("deny.lua", APP);
    let addr = free_addr();
    let server = nitr::Server::builder()
        .listen(addr)
        .handler_script(&handler)
        .builtins(nitr::Builtins::JSON | nitr::Builtins::HTTP | nitr::Builtins::FETCH)
        .workers(1)
        .build()
        .await
        .expect("build server");
    let (stop_tx, stop_rx) = tokio::sync::oneshot::channel::<()>();
    let served = tokio::spawn(server.serve_with_shutdown(async {
        let _ = stop_rx.await;
    }));
    let client = reqwest::Client::new();

    let body: serde_json::Value = client
        .get(format!("http://{addr}/try"))
        .query(&[("url", "http://127.0.0.1:9/internal")])
        .send()
        .await
        .expect("try loopback")
        .json()
        .await
        .expect("body");
    assert_eq!(body["ok"], false);
    assert!(
        body["err"]
            .as_str()
            .expect("err string")
            .contains("private or local"),
        "got: {body}"
    );

    // Metadata-endpoint style link-local addresses are refused too.
    let body: serde_json::Value = client
        .get(format!("http://{addr}/try"))
        .query(&[("url", "http://169.254.169.254/latest/meta-data/")])
        .send()
        .await
        .expect("try metadata")
        .json()
        .await
        .expect("body");
    assert_eq!(body["ok"], false);

    let _ = stop_tx.send(());
    served.await.expect("task").expect("shutdown");
    std::fs::remove_file(&handler).ok();

    // Allow-list: hosts outside fetch.allowed_hosts are refused even with
    // private networks allowed.
    let handler = write_temp("allowlist.lua", APP);
    let addr = free_addr();
    let mut cfg = nitr::Config::default();
    cfg.fetch.allowed_hosts = Some(vec!["api.example.com".into()]);
    cfg.fetch.allow_private_networks = true;

    let server = nitr::Server::builder()
        .config(cfg)
        .listen(addr)
        .handler_script(&handler)
        .builtins(nitr::Builtins::JSON | nitr::Builtins::HTTP | nitr::Builtins::FETCH)
        .workers(1)
        .build()
        .await
        .expect("build server");
    let (stop_tx, stop_rx) = tokio::sync::oneshot::channel::<()>();
    let served = tokio::spawn(server.serve_with_shutdown(async {
        let _ = stop_rx.await;
    }));

    let body: serde_json::Value = client
        .get(format!("http://{addr}/try"))
        .query(&[("url", format!("http://{addr}/whatever"))])
        .send()
        .await
        .expect("try unlisted")
        .json()
        .await
        .expect("body");
    assert_eq!(body["ok"], false);
    assert!(
        body["err"]
            .as_str()
            .expect("err string")
            .contains("allowed_hosts"),
        "got: {body}"
    );

    let _ = stop_tx.send(());
    served.await.expect("task").expect("shutdown");
    std::fs::remove_file(&handler).ok();
}
