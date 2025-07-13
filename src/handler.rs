use http_body_util::{combinators::BoxBody, BodyExt as _, Empty, Full};
use hyper::body::Bytes;
use hyper::Response;
use mlua::Table as LuaTable;
use std::convert::Infallible;
use tokio::sync::Mutex;

use crate::runtime::Runtime;
use crate::userdata::request::LuaRequest;
use crate::{Context, Result};

pub(crate) async fn handle(
    rt: &Mutex<Runtime>,
    req: LuaRequest,
) -> Result<Response<BoxBody<Bytes, Infallible>>> {
    let mut rt = rt.lock().await;
    let res = rt
        .http_fn_reload()
        .with_context(|| "Failed to reload HTTP function");
    if res.is_err() {
        // TODO: add logging
        eprintln!("{res:?}");
        return Ok(Response::builder()
            .status(hyper::StatusCode::INTERNAL_SERVER_ERROR.as_u16())
            .body(Full::new(Bytes::from("Internal Server Error")).boxed())?);
    }

    let http_fn = match rt.http_fn() {
        Some(http_fn) => http_fn,
        None => {
            return Ok(Response::builder()
                .status(hyper::StatusCode::INTERNAL_SERVER_ERROR.as_u16())
                .body(Full::new(Bytes::from("HTTP handler not being set")).boxed())?);
        }
    };

    match http_fn.call_async::<LuaTable>((rt.cfg(), req)).await {
        Ok(lua_resp) => {
            // Set status
            let status = lua_resp
                .get::<Option<u16>>("status")
                .with_context(|| "Failed to get `status`")?
                .unwrap_or(hyper::StatusCode::OK.as_u16());
            let mut resp = Response::builder().status(status);

            // Set headers
            if let Some(headers) = lua_resp
                .get::<Option<LuaTable>>("headers")
                .with_context(|| "Failed to get `headers`")?
            {
                for pair in headers.pairs::<String, String>() {
                    let (h, v) = pair?;
                    resp = resp.header(&h, v.as_bytes());
                }
            }

            // Set body
            let body = lua_resp
                .get::<Option<String>>("body")
                .with_context(|| "Failed to get `body`")?
                .map(|b| Full::new(Bytes::copy_from_slice(b.as_bytes())).boxed())
                .unwrap_or_else(|| Empty::<Bytes>::new().boxed());

            Ok(resp.body(body)?)
        }
        Err(err) => {
            // TODO: add logging
            eprintln!("{err}");
            Ok(Response::builder()
                .status(hyper::StatusCode::INTERNAL_SERVER_ERROR.as_u16())
                .body(Full::new(Bytes::from("Internal Server Error")).boxed())?)
        }
    }
}
