//! End-to-end tests for the phase-14 standard library completion:
//! `nitr.time`, `nitr.validate`, `nitr.base64`, `nitr.path`, `nitr.url`,
//! CSRF middleware, signed-cookie sessions, and the `nitr.crypto`
//! AEAD/JWT additions.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};

fn write_script(name: &str, content: &str) -> PathBuf {
    // `fs::write` truncates before writing, so a path two tests share is a
    // race; the counter keeps every call on its own file.
    static NEXT: AtomicU32 = AtomicU32::new(0);
    let id = NEXT.fetch_add(1, Ordering::Relaxed);
    let path = std::env::temp_dir().join(format!("nitr-std14-{}-{id}-{name}", std::process::id()));
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

fn header<'a>(resp: &'a nitr::testing::TestResponse, name: &str) -> Option<&'a str> {
    resp.headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case(name))
        .map(|(_, v)| v.as_str())
}

/// The cookie pair (`name=value`) from a `Set-Cookie` header, ready to
/// send back in a `Cookie` request header.
fn cookie_pair(resp: &nitr::testing::TestResponse) -> String {
    header(resp, "set-cookie")
        .expect("a Set-Cookie header")
        .split(';')
        .next()
        .expect("cookie pair")
        .to_string()
}

/// `nitr.time`, `nitr.base64`, `nitr.path` and `nitr.url` — the pure
/// utilities, exercised through a live handler.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pure_utilities_work_end_to_end() {
    let script = write_script(
        "utils.lua",
        r#"
local app = nitr.app()

app:get("/utils", function(req)
    local ts = 784887151
    local parsed = nitr.url.parse("https://api.example.com:8443/v1?x=1")
    local decoded = nitr.base64.decode(nitr.base64.encode("round trip"))
    return nitr.json({
        formatted = nitr.time.format(ts, "%Y-%m-%d %H:%M:%S"),
        iso = nitr.time.iso8601(ts),
        http_date = nitr.time.http(ts),
        reparsed = nitr.time.parse_http(nitr.time.http(ts)),
        now_plausible = nitr.time.now() > 1700000000,
        monotonic_moves = nitr.time.monotonic() >= 0,
        b64 = decoded,
        joined = nitr.path.join("/srv", "app", "logo.png"),
        ext = nitr.path.extension("C:\\files\\report.pdf"),
        normalized = nitr.path.normalize("/a/b/../c"),
        host = parsed.host,
        port = parsed.port,
        query = nitr.url.query_build({ b = "x y", a = 1 }),
    })
end)

return app
"#,
    );

    let client = client_for(
        &script,
        nitr::Builtins::JSON
            | nitr::Builtins::TIME
            | nitr::Builtins::BASE64
            | nitr::Builtins::PATH
            | nitr::Builtins::URL,
    )
    .await;

    let resp = client
        .request("GET", "/utils", &[], None)
        .await
        .expect("utils");
    assert_eq!(resp.status, 200);
    let body: serde_json::Value = serde_json::from_slice(&resp.body).expect("json");
    assert_eq!(body["formatted"], "1994-11-15 08:12:31");
    assert_eq!(body["iso"], "1994-11-15T08:12:31Z");
    assert_eq!(body["http_date"], "Tue, 15 Nov 1994 08:12:31 GMT");
    assert_eq!(body["reparsed"], 784887151);
    assert_eq!(body["now_plausible"], true);
    assert_eq!(body["monotonic_moves"], true);
    assert_eq!(body["b64"], "round trip");
    assert_eq!(body["joined"], "/srv/app/logo.png");
    assert_eq!(body["ext"], "pdf");
    assert_eq!(body["normalized"], "/a/c");
    assert_eq!(body["host"], "api.example.com");
    assert_eq!(body["port"], 8443);
    assert_eq!(body["query"], "a=1&b=x%20y");

    std::fs::remove_file(&script).ok();
}

/// `nitr.validate`: a compiled schema accepts good input, reports each bad
/// field, and strips undeclared fields.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn validation_guards_a_json_endpoint() {
    let script = write_script(
        "validate.lua",
        r#"
local app = nitr.app()

local schema = nitr.validate.schema({
    email = { type = "string", format = "email", required = true },
    age = { type = "integer", min = 0, max = 150 },
    tags = { type = "array", items = { type = "string" }, max_items = 2 },
})

app:post("/users", function(req)
    local data, err = schema:check(req:json())
    if not data then
        return nitr.error(422, { code = "VALIDATION_FAILED", fields = err.fields })
    end
    return nitr.json({ ok = true, email = data.email, role = data.role })
end)

return app
"#,
    );

    let client = client_for(
        &script,
        nitr::Builtins::JSON | nitr::Builtins::HTTP | nitr::Builtins::VALIDATE,
    )
    .await;

    let resp = client
        .request(
            "POST",
            "/users",
            &[("content-type".into(), "application/json".into())],
            Some(r#"{"email":"ada@example.com","age":36,"role":"admin"}"#.into()),
        )
        .await
        .expect("valid");
    assert_eq!(resp.status, 200);
    let body: serde_json::Value = serde_json::from_slice(&resp.body).expect("json");
    assert_eq!(body["ok"], true);
    assert_eq!(body["email"], "ada@example.com");
    // Undeclared input never reaches the handler's validated data.
    assert!(body["role"].is_null());

    let resp = client
        .request(
            "POST",
            "/users",
            &[("content-type".into(), "application/json".into())],
            Some(r#"{"age":-1,"tags":["a","b","c"]}"#.into()),
        )
        .await
        .expect("invalid");
    assert_eq!(resp.status, 422);
    let body: serde_json::Value = serde_json::from_slice(&resp.body).expect("json");
    assert_eq!(body["code"], "VALIDATION_FAILED");
    assert_eq!(body["fields"]["email"], "is required");
    assert_eq!(body["fields"]["age"], "must be >= 0");
    assert_eq!(body["fields"]["tags"], "must have at most 2 items");

    std::fs::remove_file(&script).ok();
}

/// The CSRF middleware: safe methods pass and get a token cookie, unsafe
/// methods need the token back (header or form field), and the comparison
/// rejects a missing or wrong token with 403.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn csrf_middleware_protects_unsafe_methods() {
    let script = write_script(
        "csrf.lua",
        r#"
local app = nitr.app()

app:use(nitr.csrf({ secret = "csrf-secret-0123456789" }))

app:get("/form", function(req)
    return nitr.json({ token = nitr.csrf.token(req) })
end)

app:post("/submit", function(req)
    return nitr.json({ ok = true, field = req:form().name })
end)

return app
"#,
    );

    let client = client_for(&script, nitr::Builtins::JSON | nitr::Builtins::HTTP).await;

    // A safe request passes, yields the token and its signed cookie.
    let resp = client
        .request("GET", "/form", &[], None)
        .await
        .expect("form");
    assert_eq!(resp.status, 200);
    let cookie = cookie_pair(&resp);
    let body: serde_json::Value = serde_json::from_slice(&resp.body).expect("json");
    let token = body["token"].as_str().expect("token").to_string();

    // No token: refused, and a token cookie is still issued for retry.
    let resp = client
        .request("POST", "/submit", &[], None)
        .await
        .expect("post");
    assert_eq!(resp.status, 403);
    assert!(header(&resp, "set-cookie").is_some());

    // Cookie without the echoed token: refused.
    let resp = client
        .request(
            "POST",
            "/submit",
            &[("cookie".into(), cookie.clone())],
            None,
        )
        .await
        .expect("post");
    assert_eq!(resp.status, 403);

    // Cookie plus the token in the header: accepted.
    let resp = client
        .request(
            "POST",
            "/submit",
            &[
                ("cookie".into(), cookie.clone()),
                ("x-csrf-token".into(), token.clone()),
            ],
            None,
        )
        .await
        .expect("post");
    assert_eq!(resp.status, 200);

    // The `_csrf` form field works too, and the handler can still read
    // the form afterwards (the parse is cached, the body is not re-read).
    let resp = client
        .request(
            "POST",
            "/submit",
            &[
                ("cookie".into(), cookie.clone()),
                (
                    "content-type".into(),
                    "application/x-www-form-urlencoded".into(),
                ),
            ],
            Some(format!("name=ada&_csrf={token}").into()),
        )
        .await
        .expect("post");
    assert_eq!(resp.status, 200);
    let body: serde_json::Value = serde_json::from_slice(&resp.body).expect("json");
    assert_eq!(body["field"], "ada");

    // A forged token of the right shape: refused.
    let resp = client
        .request(
            "POST",
            "/submit",
            &[
                ("cookie".into(), cookie),
                ("x-csrf-token".into(), "A".repeat(token.len())),
            ],
            None,
        )
        .await
        .expect("post");
    assert_eq!(resp.status, 403);

    std::fs::remove_file(&script).ok();
}

/// Sessions: the whole session lives in a signed cookie — set on save,
/// read back on the next request, deleted when cleared, and rejected when
/// tampered with.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sessions_round_trip_through_the_signed_cookie() {
    let script = write_script(
        "session.lua",
        r#"
local app = nitr.app()
local OPTS = { secret = "session-secret-0123456789" }

app:post("/login", function(req)
    local session = nitr.session(req, OPTS)
    session.user_id = 42
    session.name = "ada"
    local resp = nitr.json({ ok = true })
    session:save(resp)
    return resp
end)

app:get("/me", function(req)
    local session = nitr.session(req, OPTS)
    return nitr.json({ user_id = session.user_id, name = session.name })
end)

app:post("/logout", function(req)
    local session = nitr.session(req, OPTS)
    session:clear()
    local resp = nitr.json({ ok = true })
    session:save(resp)
    return resp
end)

return app
"#,
    );

    let client = client_for(&script, nitr::Builtins::JSON | nitr::Builtins::HTTP).await;

    let resp = client
        .request("POST", "/login", &[], None)
        .await
        .expect("login");
    assert_eq!(resp.status, 200);
    let set_cookie = header(&resp, "set-cookie").expect("cookie").to_string();
    assert!(set_cookie.contains("HttpOnly"), "got: {set_cookie}");
    let cookie = cookie_pair(&resp);

    let resp = client
        .request("GET", "/me", &[("cookie".into(), cookie.clone())], None)
        .await
        .expect("me");
    let body: serde_json::Value = serde_json::from_slice(&resp.body).expect("json");
    assert_eq!(body["user_id"], 42);
    assert_eq!(body["name"], "ada");

    // Without the cookie there is no session.
    let resp = client.request("GET", "/me", &[], None).await.expect("me");
    let body: serde_json::Value = serde_json::from_slice(&resp.body).expect("json");
    assert!(body["user_id"].is_null());

    // A tampered cookie verifies to nothing.
    let tampered = format!("{}x", cookie);
    let resp = client
        .request("GET", "/me", &[("cookie".into(), tampered)], None)
        .await
        .expect("me");
    let body: serde_json::Value = serde_json::from_slice(&resp.body).expect("json");
    assert!(body["user_id"].is_null());

    // Logout writes an expiring, empty cookie.
    let resp = client
        .request("POST", "/logout", &[("cookie".into(), cookie)], None)
        .await
        .expect("logout");
    let set_cookie = header(&resp, "set-cookie").expect("cookie");
    assert!(set_cookie.contains("Max-Age=0"), "got: {set_cookie}");

    std::fs::remove_file(&script).ok();
}

/// `nitr.crypto.seal`/`open` and `nitr.crypto.jwt` through a live handler.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn aead_and_jwt_work_from_lua() {
    let script = write_script(
        "crypto14.lua",
        r#"
local app = nitr.app()
local KEY = string.rep("k", 32)

app:get("/aead", function(req)
    local sealed = nitr.crypto.seal(KEY, "the plan", "user:42")
    return nitr.json({
        opened = nitr.crypto.open(KEY, sealed, "user:42"),
        wrong_aad = nitr.crypto.open(KEY, sealed, "user:7") == nil,
        tampered = nitr.crypto.open(KEY, "AAAA" .. sealed, "user:42") == nil,
    })
end)

app:get("/jwt", function(req)
    local token = nitr.crypto.jwt.sign({ sub = "42", exp = 4000000000 }, "jwt-secret")
    local claims = nitr.crypto.jwt.verify(token, "jwt-secret", { algorithms = { "HS256" } })
    local _, why = nitr.crypto.jwt.verify(token, "other-secret", { algorithms = { "HS256" } })
    return nitr.json({ sub = claims.sub, forged = why })
end)

return app
"#,
    );

    let client = client_for(&script, nitr::Builtins::JSON | nitr::Builtins::CRYPTO).await;

    let resp = client
        .request("GET", "/aead", &[], None)
        .await
        .expect("aead");
    assert_eq!(resp.status, 200);
    let body: serde_json::Value = serde_json::from_slice(&resp.body).expect("json");
    assert_eq!(body["opened"], "the plan");
    assert_eq!(body["wrong_aad"], true);
    assert_eq!(body["tampered"], true);

    let resp = client.request("GET", "/jwt", &[], None).await.expect("jwt");
    assert_eq!(resp.status, 200);
    let body: serde_json::Value = serde_json::from_slice(&resp.body).expect("json");
    assert_eq!(body["sub"], "42");
    assert_eq!(body["forged"], "invalid signature");

    std::fs::remove_file(&script).ok();
}
