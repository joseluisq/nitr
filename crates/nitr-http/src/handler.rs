use std::convert::Infallible;

use http_body_util::{combinators::BoxBody, BodyExt as _, Empty, Full};
use hyper::body::Bytes;
use hyper::{header, Method, Response, StatusCode};
use mlua::{Function, LuaString, Table as LuaTable, Value as LuaValue};

use std::sync::Arc;
use tokio::sync::Semaphore;

use crate::app::{self, AppState, Dispatch};
use crate::request::LuaRequest;
use crate::stream;
use nitr_core::{Error, Result, Runtime, RuntimeGuard, RuntimePool};

pub(crate) type HttpResponse = Response<BoxBody<Bytes, Infallible>>;

/// What a request resolves to after Rust-side routing.
enum Target {
    /// Legacy single-function script: called for every request.
    CatchAll(Function),
    /// A matched route: the composed middleware+handler chain.
    Chain {
        chain: Function,
        params: Vec<(String, String)>,
        error_fn: Option<Function>,
    },
    NotFound,
    MethodNotAllowed(Vec<Method>),
}

pub(crate) async fn handle(
    pool: &RuntimePool,
    mut req: LuaRequest,
    streams: Arc<Semaphore>,
) -> Result<HttpResponse> {
    let mut rt = pool.get().await;
    let dev_mode = rt.dev_mode();

    if dev_mode {
        if let Err(err) = app::reload_if_changed(&rt) {
            tracing::error!("failed to reload the HTTP handler: {err}");
            return error_response(&err, dev_mode);
        }
    }

    let target = match resolve(&rt, &req) {
        Ok(target) => target,
        Err(err) => {
            tracing::error!("failed to resolve the request route: {err}");
            return error_response(&err, dev_mode);
        }
    };

    match target {
        Target::NotFound => plain_response(StatusCode::NOT_FOUND, "Not Found"),
        Target::MethodNotAllowed(allowed) => {
            let mut resp = plain_response(StatusCode::METHOD_NOT_ALLOWED, "Method Not Allowed")?;
            let allowed = allowed
                .iter()
                .map(Method::as_str)
                .collect::<Vec<_>>()
                .join(", ");
            if let Ok(value) = header::HeaderValue::from_str(&allowed) {
                resp.headers_mut().insert(header::ALLOW, value);
            }
            Ok(resp)
        }
        Target::CatchAll(handler) => {
            let cfg = rt.cfg().cloned();
            match rt.call_function::<LuaTable>(handler, (cfg, req)).await {
                Ok(lua_resp) => finish(rt, lua_resp, &streams, dev_mode),
                Err(err) => {
                    tracing::error!("lua handler error: {err}");
                    error_response(&err, dev_mode)
                }
            }
        }
        Target::Chain {
            chain,
            params,
            error_fn,
        } => {
            req.2 = params;
            // The request becomes a Lua value up front so the error handler
            // can receive the same object the handler saw.
            let req_ud = rt.lua().create_userdata(req)?;
            match rt.call_function::<LuaTable>(chain, &req_ud).await {
                Ok(lua_resp) => finish(rt, lua_resp, &streams, dev_mode),
                Err(err) => {
                    tracing::error!("lua handler error: {err}");
                    if let Some(error_fn) = error_fn {
                        match rt
                            .call_function::<LuaTable>(error_fn, (err.to_string(), &req_ud))
                            .await
                        {
                            Ok(lua_resp) => match to_response(lua_resp) {
                                Ok(resp) => return Ok(resp),
                                Err(err) => {
                                    tracing::error!("invalid error-handler response: {err}")
                                }
                            },
                            Err(err) => tracing::error!("the app error handler failed: {err}"),
                        }
                    }
                    error_response(&err, dev_mode)
                }
            }
        }
    }
}

/// Completes a successful handler call: a function body becomes a
/// streaming response (moving the runtime into the producer task, subject
/// to the `max_streams` cap); anything else converts as a static response.
fn finish(
    rt: RuntimeGuard,
    lua_resp: LuaTable,
    streams: &Arc<Semaphore>,
    dev_mode: bool,
) -> Result<HttpResponse> {
    match lua_resp.raw_get::<LuaValue>("body") {
        Ok(LuaValue::Function(body_fn)) => {
            let permit = match streams.clone().try_acquire_owned() {
                Ok(permit) => permit,
                Err(_) => {
                    tracing::warn!("streaming response rejected: max_streams reached");
                    return plain_response(StatusCode::SERVICE_UNAVAILABLE, "Service Unavailable");
                }
            };
            match stream::stream_response(rt, &lua_resp, body_fn, permit) {
                Ok(resp) => Ok(resp),
                Err(err) => {
                    tracing::error!("invalid streaming response: {err}");
                    error_response(&err, dev_mode)
                }
            }
        }
        Ok(_) => match to_response(lua_resp) {
            Ok(resp) => Ok(resp),
            Err(err) => {
                tracing::error!("invalid handler response: {err}");
                error_response(&err, dev_mode)
            }
        },
        Err(err) => {
            let err = Error::from(err);
            tracing::error!("invalid handler response: {err}");
            error_response(&err, dev_mode)
        }
    }
}

/// Routes the request in Rust against this state's compiled dispatch table.
fn resolve(rt: &Runtime, req: &LuaRequest) -> Result<Target> {
    let ud = app::state(rt.lua())?;
    let state = ud.borrow::<AppState>()?;
    Ok(match &state.dispatch {
        Dispatch::CatchAll(f) => Target::CatchAll(f.clone()),
        Dispatch::App(app) => match app.router.at(req.1.uri().path()) {
            Ok(matched) => match matched.value.get(req.1.method()) {
                Some(&idx) => Target::Chain {
                    chain: app.chains[idx].clone(),
                    params: matched
                        .params
                        .iter()
                        .map(|(k, v)| (k.to_string(), v.to_string()))
                        .collect(),
                    error_fn: app.error_fn.clone(),
                },
                None => Target::MethodNotAllowed(matched.value.keys().cloned().collect()),
            },
            Err(_) => Target::NotFound,
        },
    })
}

fn plain_response(status: StatusCode, body: &'static str) -> Result<HttpResponse> {
    Ok(Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, "text/plain; charset=utf-8")
        .body(Full::new(Bytes::from_static(body.as_bytes())).boxed())?)
}

/// Converts the Lua response table `{status, headers, body}` into an HTTP
/// response. Bodies are binary-safe (Lua strings are byte strings); header
/// values may be a string or an array of strings (multi-value headers such
/// as `Set-Cookie`).
fn to_response(lua_resp: LuaTable) -> Result<HttpResponse> {
    let body = lua_resp
        .raw_get::<Option<LuaString>>("body")?
        .map(|b| Full::new(Bytes::copy_from_slice(&b.as_bytes())).boxed())
        .unwrap_or_else(|| Empty::<Bytes>::new().boxed());
    build_response(&lua_resp, body)
}

/// Builds an HTTP response from the table's status/headers/cookies around
/// an already-materialized body (static or streaming).
pub(crate) fn build_response(
    lua_resp: &LuaTable,
    body: BoxBody<Bytes, Infallible>,
) -> Result<HttpResponse> {
    use hyper::header::{HeaderName, HeaderValue};

    let status = lua_resp
        .raw_get::<Option<u16>>("status")?
        .unwrap_or(hyper::StatusCode::OK.as_u16());

    // Invalid status codes surface here.
    let mut resp = Response::builder().status(status).body(body)?;

    if let Some(headers) = lua_resp.raw_get::<Option<LuaTable>>("headers")? {
        // Insert into the header map directly (`for_each` avoids the pairs
        // iterator machinery and the response-builder indirection).
        let map = resp.headers_mut();
        let invalid_value = |name: &HeaderName| {
            mlua::Error::RuntimeError(format!("invalid value for header `{name}`"))
        };
        headers.for_each(|name: LuaString, value: LuaValue| {
            let name = HeaderName::from_bytes(&name.as_bytes()).map_err(|_| {
                mlua::Error::RuntimeError(format!("invalid header name `{}`", name.display()))
            })?;
            match value {
                LuaValue::String(v) => {
                    let v =
                        HeaderValue::from_bytes(&v.as_bytes()).map_err(|_| invalid_value(&name))?;
                    map.append(name, v);
                }
                LuaValue::Integer(v) => {
                    map.append(name, HeaderValue::from(v));
                }
                LuaValue::Table(values) => {
                    for v in values.sequence_values::<LuaString>() {
                        let v = HeaderValue::from_bytes(&v?.as_bytes())
                            .map_err(|_| invalid_value(&name))?;
                        map.append(name.clone(), v);
                    }
                }
                other => {
                    return Err(mlua::Error::RuntimeError(format!(
                        "invalid value type `{}` for header `{name}`: \
                         expected a string, an integer or an array of strings",
                        other.type_name()
                    )));
                }
            }
            Ok(())
        })?;
    }

    // Helper-built responses carry a `cookies` builder; each collected
    // entry becomes its own `Set-Cookie` header.
    match lua_resp.raw_get::<LuaValue>("cookies")? {
        LuaValue::Nil => {}
        LuaValue::UserData(ud) => {
            let cookies = ud.borrow::<nitr_lua::ResponseCookies>().map_err(|_| {
                Error::Script("the response `cookies` field is not a cookie builder".into())
            })?;
            for value in cookies.values() {
                let value = HeaderValue::from_str(&value)
                    .map_err(|_| Error::Script(format!("invalid Set-Cookie value `{value}`")))?;
                resp.headers_mut().append(hyper::header::SET_COOKIE, value);
            }
        }
        other => {
            return Err(Error::Script(format!(
                "invalid `cookies` field of type `{}` in the response table",
                other.type_name()
            )))
        }
    }

    Ok(resp)
}

/// A generic 500 that never leaks internals to clients; in development mode
/// the error (including the Lua traceback) is included for fast iteration.
fn error_response(err: &Error, dev_mode: bool) -> Result<HttpResponse> {
    let body = if dev_mode {
        format!("Internal Server Error\n\n{err}")
    } else {
        "Internal Server Error".to_string()
    };
    Ok(Response::builder()
        .status(hyper::StatusCode::INTERNAL_SERVER_ERROR)
        .header(hyper::header::CONTENT_TYPE, "text/plain; charset=utf-8")
        .body(Full::new(Bytes::from(body)).boxed())?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use mlua::Lua;

    fn eval_table(lua: &Lua, src: &str) -> LuaTable {
        lua.load(src).eval().expect("eval response table")
    }

    async fn body_bytes(resp: HttpResponse) -> Bytes {
        resp.into_body()
            .collect()
            .await
            .expect("collect")
            .to_bytes()
    }

    #[tokio::test]
    async fn defaults_to_200_and_empty_body() {
        let lua = Lua::new();
        let resp = to_response(eval_table(&lua, "{}")).expect("response");
        assert_eq!(resp.status(), 200);
        assert!(body_bytes(resp).await.is_empty());
    }

    #[tokio::test]
    async fn preserves_binary_bodies() {
        let lua = Lua::new();
        let table = eval_table(
            &lua,
            r#"{ status = 201, body = string.char(0, 255, 1) .. "x" }"#,
        );
        let resp = to_response(table).expect("response");
        assert_eq!(resp.status(), 201);
        assert_eq!(&body_bytes(resp).await[..], &[0, 255, 1, b'x']);
    }

    #[tokio::test]
    async fn supports_multi_value_and_integer_headers() {
        let lua = Lua::new();
        let table = eval_table(
            &lua,
            r#"{
                headers = {
                    ["Set-Cookie"] = { "a=1", "b=2" },
                    ["X-Limit"] = 42,
                    ["Content-Type"] = "text/plain",
                },
            }"#,
        );
        let resp = to_response(table).expect("response");
        let cookies: Vec<_> = resp.headers().get_all("set-cookie").iter().collect();
        assert_eq!(cookies, ["a=1", "b=2"]);
        assert_eq!(resp.headers()["x-limit"], "42");
        assert_eq!(resp.headers()["content-type"], "text/plain");
    }

    #[tokio::test]
    async fn rejects_invalid_headers_gracefully() {
        let lua = Lua::new();
        let bad_name = eval_table(&lua, r#"{ headers = { ["bad name"] = "x" } }"#);
        assert!(to_response(bad_name).is_err());

        let bad_type = eval_table(&lua, r#"{ headers = { ok = function() end } }"#);
        assert!(to_response(bad_type).is_err());
    }

    #[tokio::test]
    async fn error_responses_hide_details_unless_dev_mode() {
        let err = Error::Script("secret traceback".into());
        let prod = error_response(&err, false).expect("prod response");
        assert_eq!(prod.status(), 500);
        assert_eq!(&body_bytes(prod).await[..], b"Internal Server Error");

        let dev = error_response(&err, true).expect("dev response");
        let body = body_bytes(dev).await;
        assert!(String::from_utf8_lossy(&body).contains("secret traceback"));
    }
}
