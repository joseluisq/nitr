use std::convert::Infallible;

use http_body_util::{combinators::BoxBody, BodyExt as _, Empty, Full};
use hyper::body::Bytes;
use hyper::Response;
use mlua::{LuaString, Table as LuaTable, Value as LuaValue};

use crate::request::LuaRequest;
use nitr_core::RuntimePool;
use nitr_core::{Error, Result};

type HttpResponse = Response<BoxBody<Bytes, Infallible>>;

pub(crate) async fn handle(pool: &RuntimePool, req: LuaRequest) -> Result<HttpResponse> {
    let mut rt = pool.get().await;
    let dev_mode = rt.dev_mode();

    if let Err(err) = rt.http_fn_reload() {
        tracing::error!("failed to reload the HTTP handler: {err}");
        return error_response(&err, dev_mode);
    }

    match rt.call_handler(req).await {
        Ok(lua_resp) => match to_response(lua_resp) {
            Ok(resp) => Ok(resp),
            Err(err) => {
                tracing::error!("invalid handler response: {err}");
                error_response(&err, dev_mode)
            }
        },
        Err(err) => {
            tracing::error!("lua handler error: {err}");
            error_response(&err, dev_mode)
        }
    }
}

/// Converts the Lua response table `{status, headers, body}` into an HTTP
/// response. Bodies are binary-safe (Lua strings are byte strings); header
/// values may be a string or an array of strings (multi-value headers such
/// as `Set-Cookie`).
fn to_response(lua_resp: LuaTable) -> Result<HttpResponse> {
    use hyper::header::{HeaderName, HeaderValue};

    let status = lua_resp
        .raw_get::<Option<u16>>("status")?
        .unwrap_or(hyper::StatusCode::OK.as_u16());
    let body = lua_resp
        .raw_get::<Option<LuaString>>("body")?
        .map(|b| Full::new(Bytes::copy_from_slice(&b.as_bytes())).boxed())
        .unwrap_or_else(|| Empty::<Bytes>::new().boxed());

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
