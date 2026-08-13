use mlua::{Function, Lua, Value};

/// Debug function.
pub(crate) fn create_debug_fn(lua: &Lua) -> mlua::Result<Function> {
    lua.create_function(|_, value: Value| {
        tracing::debug!("[lua] {value:#?}");
        Ok(())
    })
}
