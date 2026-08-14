//! The `nitr` namespace table: the single place where every Nitr API is
//! exposed to Lua. Builtins, the application object, the configuration
//! snapshot, and Rust extension modules all mount as fields of the global
//! `nitr` table — there are no other Nitr-provided globals.

use mlua::{IntoLua, Lua, Table, Value};

use crate::error::{Error, Result};

/// Returns the global `nitr` namespace table, creating it on first use.
///
/// Every crate that exposes an API to Lua mounts it here, so the table is
/// shared: callers must only add their own fields.
pub fn nitr_table(lua: &Lua) -> Result<Table> {
    let globals = lua.globals();
    match globals.get::<Value>("nitr")? {
        Value::Table(t) => Ok(t),
        Value::Nil => {
            let t = lua.create_table()?;
            globals.set("nitr", &t)?;
            Ok(t)
        }
        other => Err(Error::Script(format!(
            "the global `nitr` must be the namespace table, found {}",
            other.type_name()
        ))),
    }
}

/// Mounts a value under `nitr.<name>`, failing when the name is already
/// taken so extensions cannot silently shadow builtins (or each other).
pub fn mount(lua: &Lua, name: &str, value: impl IntoLua) -> Result {
    let nitr = nitr_table(lua)?;
    if nitr.get::<Value>(name)? != Value::Nil {
        return Err(Error::Script(format!(
            "cannot register the module `{name}`: `nitr.{name}` already exists"
        )));
    }
    nitr.set(name, value)?;
    Ok(())
}

/// A Rust extension module: runs once per Lua state and returns the value
/// mounted at `nitr.<name>` (a table, by Lua module convention).
pub type ModuleFn = dyn Fn(&Lua) -> mlua::Result<Table> + Send + Sync;
