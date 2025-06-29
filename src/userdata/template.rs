use minijinja::{path_loader, Environment};
use mlua::{AnyUserData, ExternalResult, Lua, LuaSerdeExt, Table, UserData, UserDataMethods};
use std::sync::Arc;

use crate::error::{Context, Result};

pub(crate) struct LuaTemplate<'a>(Arc<Environment<'a>>);

impl UserData for LuaTemplate<'_> {
    fn add_methods<'lua, M: UserDataMethods<Self>>(methods: &mut M) {
        methods.add_method_mut("render", |lua, templ, args: (String, Option<Table>)| {
            let file_path = args.0;
            let data = args.1;
            let templ_store = templ.0.clone();
            let templ = templ_store
                .get_template(file_path.as_str())
                .into_lua_err()?;
            let content = templ
                .render(data)
                .with_context(|| "template_fn: error rendering template")
                .into_lua_err()?;
            lua.to_value(&content)
        });
    }
}

/// Templating function support.
pub(crate) fn create_template_fn(lua: &Lua) -> Result<AnyUserData> {
    let mut env = Environment::new();
    // TODO: add custom config for templates directory
    env.set_loader(path_loader("scripts/templates"));

    let env = Arc::new(env);
    lua.create_userdata(LuaTemplate(env))
        .with_context(|| "template_fn: failed to initialize templates")
}
