use std::sync::Arc;
use std::time::Duration;

use mlua::{ExternalResult, Function, Lua, Table, UserData, UserDataMethods};
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use reqwest::{redirect, Client as HttpClient, Method as HttpMethod, Url};

use crate::fetch::response::LuaResponse;

/// How long to wait for a TCP/TLS connection to an upstream.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// Total budget per outbound request (connect + request + response body).
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// Maximum redirects followed per outbound request.
const MAX_REDIRECTS: usize = 5;

pub(crate) struct LuaFetch(Arc<HttpClient>, HttpMethod, Url, HeaderMap);

impl UserData for LuaFetch {
    fn add_methods<M: UserDataMethods<Self>>(methods: &mut M) {
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
                .into_lua_err()?;

            Ok(LuaResponse(resp))
        });
    }
}

/// Returns the process-wide shared HTTP client, building it on first use.
/// `reqwest::Client` is internally reference-counted and designed to be
/// shared; one client means one connection pool across all Lua states.
fn shared_client() -> mlua::Result<Arc<HttpClient>> {
    static CLIENT: std::sync::OnceLock<Arc<HttpClient>> = std::sync::OnceLock::new();
    if let Some(client) = CLIENT.get() {
        return Ok(client.clone());
    }
    let client = Arc::new(
        HttpClient::builder()
            .connect_timeout(CONNECT_TIMEOUT)
            .timeout(REQUEST_TIMEOUT)
            .redirect(redirect::Policy::limited(MAX_REDIRECTS))
            .build()
            .into_lua_err()?,
    );
    Ok(CLIENT.get_or_init(|| client).clone())
}

/// HTTP fetch function.
pub(crate) fn create_fetch_fn(lua: &Lua) -> mlua::Result<Function> {
    let http_client = shared_client()?;

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
}
