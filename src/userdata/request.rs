use http_body_util::BodyExt as _;
use hyper::body::Incoming;
use hyper::Request;
use mlua::{ExternalResult, LuaSerdeExt, UserData, UserDataFields, UserDataMethods};
use serde_json::Value as SerdeValue;
use std::net::SocketAddr;

/// Wrapper around incoming request that implements UserData.
pub(crate) struct LuaRequest(pub(crate) SocketAddr, pub(crate) Request<Incoming>);

impl UserData for LuaRequest {
    fn add_fields<'lua, F: UserDataFields<Self>>(fields: &mut F) {
        fields.add_field_method_get("remote_addr", |_, req| Ok((req.0).to_string()));
        fields.add_field_method_get("method", |_, req| Ok((req.1).method().to_string()));
        fields.add_field_method_get("uri", |lua, req| {
            let table = lua.create_table()?;
            let uri = req.1.uri();
            table.set("scheme", uri.scheme_str().unwrap_or_default())?;
            table.set("host", uri.host().unwrap_or_default())?;
            table.set("port", uri.port().map_or(0, |v| v.as_u16()))?;
            table.set("path", uri.path())?;
            table.set("authority", uri.authority().map_or("", |a| a.as_str()))?;
            table.set("query", uri.query().unwrap_or_default())?;
            Ok(table)
        });
        fields.add_field_method_get("headers", |lua, req| {
            let headers = (req.1).headers();
            let table = lua.create_table()?;
            for (k, v) in headers.iter() {
                table.set(k.as_str(), v.to_str().unwrap_or_default())?;
            }
            Ok(table)
        });
    }

    fn add_methods<'lua, M: UserDataMethods<Self>>(methods: &mut M) {
        methods.add_async_method_mut("read", |lua, mut req, ()| async move {
            let reader = req.1.body_mut();
            if let Some(frame) = reader.frame().await {
                if let Some(bytes) = frame.into_lua_err()?.data_ref() {
                    return Some(lua.create_string(bytes)).transpose();
                }
            }
            Ok(None)
        });

        methods.add_async_method_mut("text", |lua, mut req, ()| async move {
            let reader = req.1.body_mut();
            let body = reader.collect().await.into_lua_err()?;
            lua.create_string(body.to_bytes())
        });

        methods.add_async_method_mut("json", |lua, mut req, ()| async move {
            let reader = req.1.body_mut();
            let collected = reader.collect().await.into_lua_err()?;
            let buf = collected.to_bytes();
            if buf.is_empty() {
                return Err(mlua::Error::external(
                    "Unexpected end of JSON input, probably request body is empty or already consumed",
                ));
            }
            let json = serde_json::from_slice::<SerdeValue>(&buf).into_lua_err()?;
            lua.to_value(&json)
        });
    }
}
