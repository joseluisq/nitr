use mlua::{
    AnyUserData, ExternalResult, Lua, LuaSerdeExt, LuaString, MetaMethod, Table, UserData,
    UserDataMethods, Value,
};
use serde_json::Value as SerdeValue;

#[derive(Default)]
pub(crate) struct LuaJson;

impl UserData for LuaJson {
    fn add_methods<M: UserDataMethods<Self>>(methods: &mut M) {
        methods.add_method_mut("encode", |lua, _, input: Value| {
            let s = serde_json::to_string(&input).into_lua_err()?;
            lua.to_value(&s)
        });

        methods.add_method_mut("decode", |lua, _, input: LuaString| {
            let v = serde_json::from_slice::<SerdeValue>(&input.as_bytes()).into_lua_err()?;
            lua.to_value(&v)
        });

        // Calling the userdata itself — `json({ ... })` — is the JSON
        // response helper: it returns a `{status, headers, body}` table.
        methods.add_meta_method(
            MetaMethod::Call,
            |lua, _, (value, status): (Value, Option<u16>)| {
                let body = serde_json::to_string(&value).into_lua_err()?;
                let table = crate::http::response_table(lua, status.unwrap_or(200))?;
                table
                    .get::<Table>("headers")?
                    .set("Content-Type", "application/json")?;
                table.set("body", body)?;
                Ok(table)
            },
        );
    }
}

/// JSON encode function via Serde.
pub(crate) fn create_json_fn(lua: &Lua) -> mlua::Result<AnyUserData> {
    lua.create_userdata(LuaJson)
}
