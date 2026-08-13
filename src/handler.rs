use http_body_util::{combinators::BoxBody, BodyExt as _, Empty, Full};
use hyper::body::Bytes;
use hyper::Response;
use mlua::{LuaString, Table as LuaTable, Value as LuaValue};
use std::convert::Infallible;

use crate::lua::request::LuaRequest;
use crate::runtime::RuntimePool;
use crate::{Error, Result};

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
