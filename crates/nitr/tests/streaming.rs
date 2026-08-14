//! End-to-end tests for streaming bodies: writer callback, iterator mode,
//! SSE, the per-chunk execution budget, the `max_streams` cap, and client
//! disconnect recovery.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;

const APP_SCRIPT: &str = r#"
local app = nitr.app()

app:get("/plain", function(req)
    return nitr.text("plain")
end)

app:get("/stream", function(req)
    return {
        status = 200,
        headers = { ["Content-Type"] = "application/json" },
        body = function(writer)
            writer:write("[")
            for i = 1, 3 do
                if i > 1 then writer:write(",") end
                writer:write(tostring(i))
            end
            writer:write("]")
        end,
    }
end)

app:get("/iterator", function(req)
    return {
        body = coroutine.wrap(function()
            coroutine.yield("chunk1 ")
            coroutine.yield("chunk2 ")
            coroutine.yield("chunk3")
        end),
    }
end)

app:get("/events", function(req)
    return nitr.sse(function(send)
        send("message", { hello = "world" })
        send("tick", "line1\nline2")
    end)
end)

app:get("/spin", function(req)
    return {
        body = function(writer)
            writer:write("a")
            while true do end -- must be stopped by the instruction hook
        end,
    }
end)

app:get("/hold", function(req)
    return {
        body = function(writer)
            local chunk = string.rep("x", 1024)
            while true do writer:write(chunk) end
        end,
    }
end)

return app
"#;

fn write_temp_script(name: &str, content: &str) -> PathBuf {
    let path = std::env::temp_dir().join(format!("nitr-streaming-{}-{name}", std::process::id()));
    std::fs::write(&path, content).expect("write temp script");
    path
}

fn free_addr() -> SocketAddr {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind port 0");
    listener.local_addr().expect("local addr")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn streaming_bodies_end_to_end() {
    let handler = write_temp_script("app.lua", APP_SCRIPT);
    let addr = free_addr();

    let mut cfg = nitr::Config {
        workers: 2,
        max_streams: Some(1),
        ..Default::default()
    };
    cfg.lua.exec_timeout_ms = 400;

    let server = nitr::Server::builder()
        .config(cfg)
        .listen(addr)
        .handler_script(&handler)
        .builtins(nitr::Builtins::JSON | nitr::Builtins::HTTP)
        .build()
        .await
        .expect("build server");

    let (stop_tx, stop_rx) = tokio::sync::oneshot::channel::<()>();
    let served = tokio::spawn(server.serve_with_shutdown(async {
        let _ = stop_rx.await;
    }));

    let base = format!("http://{addr}");
    let client = reqwest::Client::new();

    // Writer callback: chunked transfer, chunks concatenated in order.
    let resp = client
        .get(format!("{base}/stream"))
        .send()
        .await
        .expect("stream");
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.headers()["content-type"], "application/json");
    assert!(resp.headers().get("content-length").is_none());
    assert_eq!(resp.text().await.expect("stream body"), "[1,2,3]");

    // Iterator mode via coroutine.wrap.
    let resp = client
        .get(format!("{base}/iterator"))
        .send()
        .await
        .expect("iterator");
    assert_eq!(resp.status(), 200);
    assert_eq!(
        resp.text().await.expect("iterator body"),
        "chunk1 chunk2 chunk3"
    );

    // SSE: headers and wire format (multi-line data → multiple data: lines).
    let resp = client
        .get(format!("{base}/events"))
        .send()
        .await
        .expect("events");
    assert_eq!(resp.headers()["content-type"], "text/event-stream");
    assert_eq!(resp.headers()["cache-control"], "no-cache");
    let body = resp.text().await.expect("events body");
    assert!(body.contains("event: message\ndata: {\"hello\":\"world\"}\n\n"));
    assert!(body.contains("event: tick\ndata: line1\ndata: line2\n\n"));

    // A CPU-bound loop mid-stream is stopped by the instruction hook: the
    // client sees the chunks written so far, and the state recovers.
    let resp = client
        .get(format!("{base}/spin"))
        .send()
        .await
        .expect("spin");
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.text().await.expect("spin body"), "a");
    let resp = client
        .get(format!("{base}/plain"))
        .send()
        .await
        .expect("plain after spin");
    assert_eq!(resp.status(), 200);

    // max_streams = 1: while one stream is live, a second streaming
    // response is rejected with 503 but plain requests still work.
    let held = client
        .get(format!("{base}/hold"))
        .send()
        .await
        .expect("hold");
    assert_eq!(held.status(), 200);
    tokio::time::sleep(Duration::from_millis(200)).await;

    let resp = client
        .get(format!("{base}/stream"))
        .send()
        .await
        .expect("stream while held");
    assert_eq!(resp.status(), 503);

    let resp = client
        .get(format!("{base}/plain"))
        .send()
        .await
        .expect("plain while held");
    assert_eq!(resp.status(), 200);

    // Dropping the client cancels the held stream, frees its slot and
    // returns its state to the pool.
    drop(held);
    tokio::time::sleep(Duration::from_millis(500)).await;

    let resp = client
        .get(format!("{base}/stream"))
        .send()
        .await
        .expect("stream after release");
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.text().await.expect("body after release"), "[1,2,3]");

    let _ = stop_tx.send(());
    served
        .await
        .expect("server task")
        .expect("server shutdown cleanly");

    std::fs::remove_file(&handler).ok();
}
