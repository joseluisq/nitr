use mlua::{Function, Lua, Value};

/// Debug function.
pub(crate) fn create_debug_fn(lua: &Lua) -> mlua::Result<Function> {
    lua.create_function(|_, value: Value| {
        tracing::debug!("[lua] {value:#?}");
        Ok(())
    })
}

/// Builds the structured error value the error model hands to Lua: the
/// table `on_error` receives and `nitr.errinfo` returns. A plain table (it
/// serializes to JSON for free) that stringifies — and concatenates — as
/// the concise `kind: message (source:line)` form via a cached metatable.
pub fn error_lua_value(lua: &Lua, info: &nitr_core::ErrorInfo) -> mlua::Result<mlua::Table> {
    let t = lua.create_table()?;
    t.raw_set("message", info.message.as_str())?;
    t.raw_set("kind", info.kind)?;
    if let Some(source) = &info.source {
        t.raw_set("source", source.as_str())?;
    }
    if let Some(line) = info.line {
        t.raw_set("line", line)?;
    }
    if let Some(module) = &info.module {
        t.raw_set("module", module.as_str())?;
    }
    if let Some(traceback) = &info.traceback {
        t.raw_set("traceback", traceback.as_str())?;
    }
    if !info.cause.is_empty() {
        let causes = lua.create_table_from(
            info.cause
                .iter()
                .enumerate()
                .map(|(i, c)| (i + 1, c.as_str())),
        )?;
        t.raw_set("cause", causes)?;
    }
    t.raw_set("__concise", info.concise())?;
    // For console prints: ANSI-colored on a terminal, identical to the
    // concise form otherwise. `tostring`/`..` stay plain deliberately —
    // those strings may end up in HTTP bodies and log files.
    let pretty = if console_wants_color() {
        info.concise_colored()
    } else {
        info.concise()
    };
    t.raw_set("pretty", pretty)?;
    t.set_metatable(Some(error_metatable(lua)?))?;
    Ok(t)
}

/// Whether `print`-style console output should carry color: stdout is a
/// terminal and `NO_COLOR` is unset.
fn console_wants_color() -> bool {
    use std::io::IsTerminal as _;
    std::io::stdout().is_terminal() && std::env::var_os("NO_COLOR").is_none_or(|v| v.is_empty())
}

/// The shared metatable for error values, built once per state:
/// `__tostring` for `tostring(err)` and `__concat` so `"prefix: " .. err`
/// works directly in a log line.
fn error_metatable(lua: &Lua) -> mlua::Result<mlua::Table> {
    if let Ok(mt) = lua.named_registry_value::<mlua::Table>("nitr.error_mt") {
        return Ok(mt);
    }
    let mt = lua.create_table()?;
    mt.set(
        "__tostring",
        lua.create_function(|_, t: mlua::Table| t.raw_get::<String>("__concise"))?,
    )?;
    // Either operand may be the error value; Lua's own `tostring` applies
    // the `__tostring` above for whichever side is.
    mt.set(
        "__concat",
        lua.create_function(|lua, (a, b): (Value, Value)| {
            let tostring: Function = lua.globals().get("tostring")?;
            let a: String = tostring.call(a)?;
            let b: String = tostring.call(b)?;
            Ok(a + &b)
        })?,
    )?;
    lua.set_named_registry_value("nitr.error_mt", &mt)?;
    Ok(mt)
}

/// `nitr.errinfo(err)`: classifies whatever `pcall` caught into the same
/// structured error value `on_error` receives. A Rust-side error arrives
/// with its full chain (`Value::Error`); a Lua error arrives as a
/// position-prefixed string; anything else is stringified. Idempotent on
/// values that are already structured.
pub(crate) fn create_errinfo_fn(lua: &Lua) -> mlua::Result<Function> {
    lua.create_function(|lua, value: Value| {
        let info = match &value {
            Value::Error(err) => {
                nitr_core::ErrorInfo::from_error(&nitr_core::Error::Lua((**err).clone()))
            }
            Value::String(s) => nitr_core::ErrorInfo::from_message(&s.to_string_lossy()),
            Value::Table(t) if t.raw_get::<Option<String>>("__concise")?.is_some() => {
                return Ok(t.clone());
            }
            other => {
                let tostring: Function = lua.globals().get("tostring")?;
                let text: String = tostring.call(other)?;
                nitr_core::ErrorInfo::from_message(&text)
            }
        };
        error_lua_value(lua, &info)
    })
}
