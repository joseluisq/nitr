use mlua::{
    AnyUserData, ExternalResult, Lua, LuaSerdeExt, String as LuaString, UserData, UserDataMethods,
    Value,
};
use serde_json::Value as SerdeValue;

use crate::error::{Context, Result};

#[derive(Default)]
pub(crate) struct LuaJson;

impl UserData for LuaJson {
    fn add_methods<'lua, M: UserDataMethods<Self>>(methods: &mut M) {
        methods.add_method_mut("encode", |lua, _, input: Value| {
            let s = serde_json::to_string(&input)
                .with_context(|| "json_fn: error encoding lua value to string")
                .into_lua_err()?;
            lua.to_value(&s)
        });

        methods.add_method_mut("decode", |lua, _, input: LuaString| {
            let v = serde_json::from_slice::<SerdeValue>(&input.as_bytes())
                .with_context(|| "json_fn: error decoding string to lua value")
                .into_lua_err()?;
            lua.to_value(&v)
        });
    }
}

/// JSON encode function via Serde.
pub(crate) fn create_json_fn(lua: &Lua) -> Result<AnyUserData> {
    lua.create_userdata(LuaJson)
        .with_context(|| "json_fn: failed to initialize json function")
}
