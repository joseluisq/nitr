use bytes::BytesMut;
use mlua::{ExternalResult, LuaSerdeExt, UserData, UserDataFields, UserDataMethods};
use reqwest::Response;
use serde_json::Value as SerdeValue;

pub(crate) struct LuaResponse(pub(crate) Response);

impl UserData for LuaResponse {
    fn add_fields<'lua, F: UserDataFields<Self>>(fields: &mut F) {
        fields.add_field_method_get("status", |_, resp| Ok(resp.0.status().as_u16()));

        fields.add_field_method_get("url", |lua, resp| {
            let table = lua.create_table()?;
            let url = resp.0.url();

            table
                .set("scheme", url.scheme().to_string())
                .into_lua_err()?;
            table
                .set("host", url.host_str().unwrap_or_default())
                .into_lua_err()?;
            table
                .set("port", url.port().unwrap_or_default())
                .into_lua_err()?;
            table.set("path", url.path()).into_lua_err()?;
            table
                .set("authority", url.authority().to_string())
                .into_lua_err()?;
            table
                .set("query", url.query().unwrap_or_default())
                .into_lua_err()?;

            Ok(table)
        });

        fields.add_field_method_get("headers", |lua, resp| {
            let headers = resp.0.headers();
            let table = lua.create_table().into_lua_err()?;
            for (k, v) in headers.iter() {
                table
                    .set(k.as_str(), v.to_str().unwrap_or_default())
                    .into_lua_err()?;
            }
            Ok(table)
        });

        fields.add_field_method_get("content_length", |_, resp| Ok(resp.0.content_length()));
    }

    fn add_methods<'lua, M: UserDataMethods<Self>>(methods: &mut M) {
        methods.add_async_method_mut("read", |lua, mut resp, ()| async move {
            if let Some(chunk) = resp.0.chunk().await.into_lua_err()? {
                return Some(lua.create_string(chunk)).transpose();
            }
            Ok(None)
        });

        methods.add_async_method_mut("json", |lua, mut resp, ()| async move {
            let len = resp.0.content_length().unwrap_or_default() as usize;
            let mut buf = BytesMut::with_capacity(len);
            while let Some(b) = resp.0.chunk().await.into_lua_err()? {
                buf.extend_from_slice(&b);
            }
            if buf.is_empty() {
                return Err(mlua::Error::external(
                    "Unexpected end of JSON input, probably response body is empty or already consumed",
                ));
            }
            let json = serde_json::from_slice::<SerdeValue>(&buf.freeze()).into_lua_err()?;
            lua.to_value(&json)
        });

        methods.add_async_method_mut("text", |lua, mut resp, ()| async move {
            let len = resp.0.content_length().unwrap_or_default() as usize;
            let mut buf = BytesMut::with_capacity(len);
            while let Some(b) = resp.0.chunk().await.into_lua_err()? {
                buf.extend_from_slice(&b);
            }
            lua.create_string(buf.freeze())
        });
    }
}
