use mlua::{Function, Lua, Value};

use crate::error::{Context, Result};

/// Debug function.
pub(crate) fn create_debug_fn(lua: &Lua) -> Result<Function> {
    lua.create_function(|_, value: Value| {
        println!("[debug] {value:#?}");
        Ok(())
    })
    .with_context(|| "debug_fn: error printing the value")
}
