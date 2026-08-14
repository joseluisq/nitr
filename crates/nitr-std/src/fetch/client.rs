//! The `fetch` builtin: outbound HTTP requests with an options table,
//! policy-checked redirects, and `await_all(...)` structured concurrency.
//!
//! `fetch(method, url, opts?)` returns an *unsent* request handle;
//! `handle:send()` performs it, and `await_all(h1, h2, ...)` performs
//! several concurrently, returning their responses in argument order.

use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use mlua::{
    AnyUserData, ExternalResult, Function, Lua, Table, UserData, UserDataMethods, Value, Variadic,
};
use reqwest::header::{HeaderMap, HeaderName, HeaderValue, CONTENT_TYPE, LOCATION};
use reqwest::{redirect, Client as HttpClient, Method as HttpMethod, StatusCode, Url};

use crate::fetch::policy::{check_url, FetchOptions};
use crate::fetch::response::LuaResponse;

/// How long to wait for a TCP/TLS connection to an upstream.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// Total budget per outbound request (connect + request + response body).
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// Maximum redirects followed per outbound request.
const MAX_REDIRECTS: usize = 5;

/// Everything needed to (re-)issue one outbound request.
#[derive(Clone)]
struct RequestSpec {
    method: HttpMethod,
    url: Url,
    headers: HeaderMap,
    body: Option<Bytes>,
    timeout: Option<Duration>,
}

/// An unsent outbound request handle.
pub(crate) struct LuaFetch {
    client: Arc<HttpClient>,
    spec: RequestSpec,
    opts: Arc<FetchOptions>,
}

impl UserData for LuaFetch {
    fn add_methods<M: UserDataMethods<Self>>(methods: &mut M) {
        methods.add_async_method("send", |_, this, ()| {
            let client = this.client.clone();
            let spec = this.spec.clone();
            let opts = this.opts.clone();
            async move { execute(&client, spec, &opts).await }
        });
    }
}

/// Performs a request under the fetch policy, following redirects manually
/// so every hop is re-validated ([`check_url`]).
async fn execute(
    client: &HttpClient,
    spec: RequestSpec,
    opts: &FetchOptions,
) -> mlua::Result<LuaResponse> {
    let RequestSpec {
        mut method,
        mut url,
        headers,
        mut body,
        timeout,
    } = spec;

    let mut hops = 0usize;
    loop {
        check_url(&url, opts).await?;

        let mut builder = client
            .request(method.clone(), url.clone())
            .headers(headers.clone());
        if let Some(bytes) = &body {
            builder = builder.body(bytes.clone());
        }
        if let Some(timeout) = timeout {
            builder = builder.timeout(timeout);
        }
        let resp = builder.send().await.into_lua_err()?;

        if !resp.status().is_redirection() {
            return Ok(LuaResponse::new(resp, opts.max_response_bytes));
        }
        let Some(location) = resp
            .headers()
            .get(LOCATION)
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned)
        else {
            // A redirect status without a Location header is a final
            // response as far as the client is concerned.
            return Ok(LuaResponse::new(resp, opts.max_response_bytes));
        };
        hops += 1;
        if hops > MAX_REDIRECTS {
            return Err(mlua::Error::RuntimeError(format!(
                "fetch exceeded {MAX_REDIRECTS} redirects for `{url}`"
            )));
        }
        url = url.join(&location).into_lua_err()?;
        // Like browsers: 301/302/303 switch to GET and drop the body;
        // 307/308 preserve the method and body.
        if matches!(
            resp.status(),
            StatusCode::MOVED_PERMANENTLY | StatusCode::FOUND | StatusCode::SEE_OTHER
        ) && method != HttpMethod::GET
            && method != HttpMethod::HEAD
        {
            method = HttpMethod::GET;
            body = None;
        }
    }
}

/// Returns the process-wide shared HTTP client, building it on first use.
/// `reqwest::Client` is internally reference-counted and designed to be
/// shared; one client means one connection pool across all Lua states.
/// Redirects are disabled here — [`execute`] follows them itself so the
/// policy applies per hop.
fn shared_client() -> mlua::Result<Arc<HttpClient>> {
    static CLIENT: std::sync::OnceLock<Arc<HttpClient>> = std::sync::OnceLock::new();
    if let Some(client) = CLIENT.get() {
        return Ok(client.clone());
    }
    let client = Arc::new(
        HttpClient::builder()
            .connect_timeout(CONNECT_TIMEOUT)
            .timeout(REQUEST_TIMEOUT)
            .redirect(redirect::Policy::none())
            .build()
            .into_lua_err()?,
    );
    Ok(CLIENT.get_or_init(|| client).clone())
}

/// The option keys that mark the third `fetch` argument as an options
/// table; any other table is treated as plain headers (the pre-options
/// call form).
const OPTION_KEYS: &[&str] = &["headers", "query", "json", "body", "timeout"];

fn parse_spec(method: String, url: String, arg: Option<Table>) -> mlua::Result<RequestSpec> {
    let method = HttpMethod::from_bytes(method.to_uppercase().as_bytes()).into_lua_err()?;
    let mut url = url.parse::<Url>().into_lua_err()?;
    let mut headers = HeaderMap::new();
    let mut body = None;
    let mut timeout = None;

    if let Some(table) = arg {
        let is_options = OPTION_KEYS
            .iter()
            .any(|k| table.contains_key(*k).unwrap_or(false));
        if is_options {
            if let Some(header_table) = table.get::<Option<Table>>("headers")? {
                fill_headers(&mut headers, &header_table)?;
            }
            if let Some(query) = table.get::<Option<Table>>("query")? {
                let mut pairs = url.query_pairs_mut();
                for pair in query.pairs::<String, String>() {
                    let (k, v) = pair?;
                    pairs.append_pair(&k, &v);
                }
            }
            if let Some(value) = table.get::<Option<Value>>("json")? {
                let bytes = serde_json::to_vec(&value).into_lua_err()?;
                headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
                body = Some(Bytes::from(bytes));
            }
            if body.is_none() {
                if let Some(raw) = table.get::<Option<mlua::LuaString>>("body")? {
                    body = Some(Bytes::copy_from_slice(&raw.as_bytes()));
                }
            }
            if let Some(secs) = table.get::<Option<f64>>("timeout")? {
                timeout = Some(Duration::from_secs_f64(secs.max(0.0)));
            }
        } else {
            fill_headers(&mut headers, &table)?;
        }
    }

    Ok(RequestSpec {
        method,
        url,
        headers,
        body,
        timeout,
    })
}

fn fill_headers(headers: &mut HeaderMap, table: &Table) -> mlua::Result<()> {
    for pair in table.pairs::<String, String>() {
        let (k, v) = pair.into_lua_err()?;
        headers.insert(
            HeaderName::from_bytes(k.as_bytes()).into_lua_err()?,
            HeaderValue::from_bytes(v.as_bytes()).into_lua_err()?,
        );
    }
    Ok(())
}

/// HTTP fetch function: `fetch(method, url, opts?)` → request handle.
pub(crate) fn create_fetch_fn(lua: &Lua, opts: Arc<FetchOptions>) -> mlua::Result<Function> {
    let http_client = shared_client()?;

    lua.create_function(
        move |_, (method, url, arg): (String, String, Option<Table>)| {
            Ok(LuaFetch {
                client: http_client.clone(),
                spec: parse_spec(method, url, arg)?,
                opts: opts.clone(),
            })
        },
    )
}

/// `await_all(req1, req2, ...)`: sends the given unsent fetch handles
/// concurrently and returns their responses in the same order. Fails as a
/// whole if any request fails.
pub(crate) fn create_await_all_fn(lua: &Lua, opts: Arc<FetchOptions>) -> mlua::Result<Function> {
    lua.create_async_function(move |_, handles: Variadic<AnyUserData>| {
        let max_concurrent = opts.max_concurrent;
        async move {
            if handles.len() > max_concurrent {
                return Err(mlua::Error::RuntimeError(format!(
                    "await_all called with {} requests, fetch.max_concurrent is {max_concurrent}",
                    handles.len()
                )));
            }
            // Copy each handle's request out before awaiting so no Lua
            // borrow lives across a suspension point.
            let mut requests = Vec::with_capacity(handles.len());
            for handle in handles.iter() {
                let fetch = handle.borrow::<LuaFetch>().map_err(|_| {
                    mlua::Error::RuntimeError("await_all expects fetch(...) request handles".into())
                })?;
                requests.push((fetch.client.clone(), fetch.spec.clone(), fetch.opts.clone()));
            }
            let responses =
                futures_util::future::try_join_all(requests.into_iter().map(
                    |(client, spec, opts)| async move { execute(&client, spec, &opts).await },
                ))
                .await?;
            Ok(Variadic::from_iter(responses))
        }
    })
}
