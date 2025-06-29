use std::sync::Arc;

use hyper::{
    header::{HeaderName, HeaderValue},
    HeaderMap,
};
use mlua::{ExternalResult, Function, Lua, Table, UserData, UserDataMethods};
use reqwest::{Client as HttpClient, Method as HttpMethod, Url};
// use serde_json::Value as SerdeValue;

use crate::error::{Context, Result};
use crate::userdata::response::LuaResponse;

pub(crate) struct LuaFetch(Arc<HttpClient>, HttpMethod, Url, HeaderMap);

impl UserData for LuaFetch {
    fn add_methods<'lua, M: UserDataMethods<Self>>(methods: &mut M) {
        methods.add_async_method_mut("send", |_, args, ()| async move {
            let http_client = args.0.clone();
            let method = args.1.clone();
            let url = args.2.clone();
            let headers = args.3.clone();

            let resp = http_client
                .request(method, url)
                .headers(headers)
                .send()
                .await
                // .and_then(|resp| resp.error_for_status())
                .into_lua_err()?;

            Ok(LuaResponse(resp))
        });
    }
}

/// HTTP fetch function.
pub(crate) fn create_fetch_fn(lua: &Lua) -> Result<Function> {
    let http_client = HttpClient::builder().build().into_lua_err()?;
    let http_client = Arc::new(http_client);

    lua.create_async_function(move |_, args: (String, String, Option<Table>)| {
        let http_client = http_client.clone();

        async move {
            let method = HttpMethod::from_bytes(args.0.to_uppercase().as_bytes()).into_lua_err()?;
            let url = args.1.parse::<Url>().into_lua_err()?;
            let headers_opt = args.2;

            let mut headers = HeaderMap::new();
            if let Some(table) = headers_opt {
                for pair in table.pairs::<String, String>() {
                    let (k, v) = pair.into_lua_err()?;
                    headers.insert(
                        HeaderName::from_bytes(k.as_bytes()).into_lua_err()?,
                        HeaderValue::from_bytes(v.as_bytes()).into_lua_err()?,
                    );
                }
            }

            Ok(LuaFetch(http_client, method, url, headers))
        }
    })
    .with_context(|| "error fetching the response")
}
