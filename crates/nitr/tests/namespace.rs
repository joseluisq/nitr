//! End-to-end tests for the phase-8 extension boundary: the `nitr.*`
//! namespace is the only surface Nitr exposes to Lua, Rust extension
//! modules mount beside the builtins, and the crypto/auth primitives.

use std::path::PathBuf;

fn write_script(name: &str, content: &str) -> PathBuf {
    let path = std::env::temp_dir().join(format!("nitr-ns-{}-{name}", std::process::id()));
    std::fs::write(&path, content).expect("write temp script");
    path
}

async fn client_for(script: &PathBuf, builtins: nitr::Builtins) -> nitr::testing::TestClient {
    let server = nitr::Server::builder()
        .handler_script(script)
        .builtins(builtins)
        .workers(1)
        .build()
        .await
        .expect("build server");
    server.test_client()
}

/// Nitr contributes exactly one global — `nitr` — and every builtin hangs
/// off it. Nothing is intermixed with the Lua standard library.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn only_the_nitr_global_is_registered() {
    let script = write_script(
        "globals.lua",
        r#"
local app = nitr.app()

app:get("/globals", function(req)
    -- Every name Nitr could have leaked as a bare global.
    local leaked = {}
    for _, name in ipairs({
        "json", "fetch", "await_all", "template", "conn", "db", "dbg",
        "text", "html", "redirect", "status", "negotiate", "sse",
        "http", "log", "crypto", "auth", "test",
    }) do
        if _G[name] ~= nil then
            table.insert(leaked, name)
        end
    end
    return nitr.json({ leaked = leaked })
end)

app:get("/members", function(req)
    local members = {}
    for _, name in ipairs({
        "app", "json", "text", "html", "redirect", "status",
        "negotiate", "sse", "error", "log", "dbg", "crypto", "auth",
    }) do
        members[name] = type(nitr[name])
    end
    return nitr.json(members)
end)

return app
"#,
    );

    let client = client_for(
        &script,
        nitr::Builtins::JSON
            | nitr::Builtins::HTTP
            | nitr::Builtins::LOG
            | nitr::Builtins::DEBUG
            | nitr::Builtins::CRYPTO,
    )
    .await;

    let resp = client
        .request("GET", "/globals", &[], None)
        .await
        .expect("globals");
    let body: serde_json::Value = serde_json::from_slice(&resp.body).expect("json");
    // An empty Lua table encodes as `{}`, so treat both shapes as empty.
    let leaked = body["leaked"].as_array().map(Vec::as_slice).unwrap_or(&[]);
    assert!(
        leaked.is_empty(),
        "Nitr must not register bare globals, found: {leaked:?}"
    );

    // …and everything is reachable through the namespace instead.
    let resp = client
        .request("GET", "/members", &[], None)
        .await
        .expect("members");
    let members: serde_json::Value = serde_json::from_slice(&resp.body).expect("json");
    for name in [
        "app",
        "json",
        "text",
        "html",
        "redirect",
        "status",
        "negotiate",
        "sse",
        "error",
        "log",
        "dbg",
        "crypto",
        "auth",
    ] {
        let kind = members[name].as_str().unwrap_or("nil");
        assert_ne!(kind, "nil", "nitr.{name} must exist");
    }
    // `nitr.json` is callable userdata (helper + codec); the rest are
    // functions or tables.
    assert_eq!(members["json"], "userdata");
    assert_eq!(members["log"], "table");
    assert_eq!(members["text"], "function");

    std::fs::remove_file(&script).ok();
}

/// The handler script must return a `nitr.app()`: the legacy
/// `function(cfg, req)` catch-all style is gone, and the failure is a
/// startup error, not a per-request surprise.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn legacy_function_handlers_are_rejected() {
    let script = write_script(
        "legacy.lua",
        "return function(cfg, req) return { status = 200, body = 'legacy' } end",
    );

    let err = nitr::Server::builder()
        .handler_script(&script)
        .builtins(nitr::Builtins::JSON)
        .workers(1)
        .build()
        .await
        .expect_err("a plain function handler must be rejected");
    let msg = err.to_string();
    assert!(
        msg.contains("must return a nitr.app()"),
        "unexpected error: {msg}"
    );

    std::fs::remove_file(&script).ok();
}

/// Rust extension modules mount at `nitr.<name>` in every pooled state and
/// are indistinguishable from builtins on the Lua side.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rust_modules_mount_on_the_namespace() {
    let script = write_script(
        "module.lua",
        r#"
local app = nitr.app()

app:get("/greet/:name", function(req)
    return nitr.json({
        greeting = nitr.demo.greet(req.params.name),
        counter = nitr.demo.next(),
        kind = type(nitr.demo),
    })
end)

return app
"#,
    );

    let counter = std::sync::Arc::new(std::sync::atomic::AtomicI64::new(0));
    let shared = counter.clone();
    let server = nitr::Server::builder()
        .handler_script(&script)
        .builtins(nitr::Builtins::JSON)
        .module("demo", move |lua| {
            let table = lua.create_table()?;
            table.set(
                "greet",
                lua.create_function(|_, name: String| Ok(format!("Hello, {name}!")))?,
            )?;
            let shared = shared.clone();
            table.set(
                "next",
                lua.create_function(move |_, ()| {
                    Ok(shared.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1)
                })?,
            )?;
            Ok(table)
        })
        // Two states: the module closure must run for each of them.
        .workers(2)
        .build()
        .await
        .expect("build server");
    let client = server.test_client();

    let resp = client
        .request("GET", "/greet/nitr", &[], None)
        .await
        .expect("request");
    assert_eq!(resp.status, 200);
    let body: serde_json::Value = serde_json::from_slice(&resp.body).expect("json");
    assert_eq!(body["greeting"], "Hello, nitr!");
    assert_eq!(body["kind"], "table");

    // The Rust-side state is shared across pooled states.
    let resp = client
        .request("GET", "/greet/again", &[], None)
        .await
        .expect("request");
    let body: serde_json::Value = serde_json::from_slice(&resp.body).expect("json");
    assert_eq!(body["counter"], 2);

    std::fs::remove_file(&script).ok();
}

/// A module may not shadow a builtin (or another module): the collision is
/// a build-time error.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn module_name_collisions_fail_at_build_time() {
    let script = write_script(
        "collide.lua",
        "local app = nitr.app()\napp:get('/', function(req) return nitr.text('ok') end)\nreturn app",
    );

    for name in ["json", "app"] {
        let err = nitr::Server::builder()
            .handler_script(&script)
            .builtins(nitr::Builtins::JSON | nitr::Builtins::HTTP)
            .module(name, |lua| lua.create_table())
            .workers(1)
            .build()
            .await
            .expect_err("a colliding module name must be rejected");
        assert!(
            err.to_string().contains("already exists"),
            "unexpected error for `{name}`: {err}"
        );
    }

    // Two modules with the same name collide with each other too.
    let err = nitr::Server::builder()
        .handler_script(&script)
        .builtins(nitr::Builtins::HTTP)
        .module("twice", |lua| lua.create_table())
        .module("twice", |lua| lua.create_table())
        .workers(1)
        .build()
        .await
        .expect_err("duplicate module names must be rejected");
    assert!(err.to_string().contains("already exists"), "got: {err}");

    std::fs::remove_file(&script).ok();
}

/// `nitr.crypto` and `nitr.auth`: hashing, HMAC, randomness, constant-time
/// comparison, argon2id passwords, and Authorization header parsing.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn crypto_and_auth_primitives() {
    let script = write_script(
        "crypto.lua",
        r#"
local app = nitr.app()

app:get("/digest", function(req)
    local token = nitr.crypto.random_bytes(32)
    return nitr.json({
        sha256 = nitr.crypto.sha256("abc"),
        hmac = nitr.crypto.hmac_sha256("key", "abc"),
        random_len = #token,
        random_differs = nitr.crypto.random_bytes(32) ~= token,
        eq_same = nitr.crypto.constant_time_eq("secret", "secret"),
        eq_diff = nitr.crypto.constant_time_eq("secret", "secrez"),
    })
end)

app:post("/password", function(req)
    local hash = nitr.crypto.password_hash("hunter2")
    return nitr.json({
        prefix = hash:sub(1, 10),
        ok = nitr.crypto.password_verify("hunter2", hash),
        bad = nitr.crypto.password_verify("hunter3", hash),
        garbage = nitr.crypto.password_verify("hunter2", "not-a-hash"),
    })
end)

app:get("/auth", function(req)
    local user, pass = nitr.auth.basic(req)
    return nitr.json({
        bearer = nitr.auth.bearer(req) or "none",
        user = user or "none",
        pass = pass or "none",
    })
end)

return app
"#,
    );

    let client = client_for(&script, nitr::Builtins::JSON | nitr::Builtins::CRYPTO).await;

    let resp = client
        .request("GET", "/digest", &[], None)
        .await
        .expect("digest");
    let body: serde_json::Value = serde_json::from_slice(&resp.body).expect("json");
    assert_eq!(
        body["sha256"],
        "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
    );
    assert_eq!(
        body["hmac"],
        "9c196e32dc0175f86f4b1cb89289d6619de6bee699e4c378e68309ed97a1a6ab"
    );
    assert_eq!(body["random_len"], 32);
    assert_eq!(body["random_differs"], true);
    assert_eq!(body["eq_same"], true);
    assert_eq!(body["eq_diff"], false);

    let resp = client
        .request("POST", "/password", &[], None)
        .await
        .expect("password");
    let body: serde_json::Value = serde_json::from_slice(&resp.body).expect("json");
    assert_eq!(body["prefix"], "$argon2id$");
    assert_eq!(body["ok"], true);
    assert_eq!(body["bad"], false);
    assert_eq!(body["garbage"], false);

    // Bearer and Basic parsing off the live request object.
    let resp = client
        .request(
            "GET",
            "/auth",
            &[("authorization".into(), "Bearer t0ken".into())],
            None,
        )
        .await
        .expect("bearer");
    let body: serde_json::Value = serde_json::from_slice(&resp.body).expect("json");
    assert_eq!(body["bearer"], "t0ken");
    assert_eq!(body["user"], "none");

    // "ada:lovelace" base64-encoded.
    let resp = client
        .request(
            "GET",
            "/auth",
            &[("authorization".into(), "Basic YWRhOmxvdmVsYWNl".into())],
            None,
        )
        .await
        .expect("basic");
    let body: serde_json::Value = serde_json::from_slice(&resp.body).expect("json");
    assert_eq!(body["user"], "ada");
    assert_eq!(body["pass"], "lovelace");
    assert_eq!(body["bearer"], "none");

    std::fs::remove_file(&script).ok();
}
