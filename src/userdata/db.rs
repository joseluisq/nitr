use mlua::{AnyUserData, Lua, Table, UserData, UserDataMethods, Value};
use rusqlite::Connection;
use std::sync::{Arc, Mutex};

use crate::error::{Context, Result};

pub(crate) struct LuaDatabase(Arc<Mutex<Connection>>);

impl UserData for LuaDatabase {
    fn add_methods<'lua, M: UserDataMethods<Self>>(methods: &mut M) {
        methods.add_method_mut("execute", |_, db, args: (String, Option<Table>)| {
            let sql = args.0;
            let conn = db.0.clone();

            // TODO: figure it out how to pass arguments to `execute`
            // let params = args.1;
            // let mut positional_params: Vec<Value> = vec![];
            // let mut named_params: Vec<(String, Value)> = vec![];

            // if let Some(params_table) = params {
            //     let len = params_table.raw_len();
            //     if len > 0 {
            //         // Assume positional parameters if it has a sequence part
            //         for i in 1..=len {
            //             positional_params.push(params_table.raw_get(i)?);
            //         }
            //     } else {
            //         // Assume named parameters if it has no sequence part
            //         for pair in params_table.pairs::<String, Value>() {
            //             let (k, v) = pair?;
            //             named_params.push((k, v));
            //         }
            //     }
            // }

            match conn.clone().lock().unwrap().execute(sql.as_str(), ()) {
                Ok(affected) => Ok(Value::Integer(affected as i64)),
                Err(err) => Err(mlua::Error::RuntimeError(err.to_string())),
            }
        });
    }
}

/// Templating function support.
pub(crate) fn create_database_fn(lua: &Lua, path: &str) -> Result<AnyUserData> {
    let db =
        Connection::open(path).with_context(|| format!("Failed to open database at {}", path))?;
    let conn = Arc::new(Mutex::new(db));
    lua.create_userdata(LuaDatabase(conn))
        .with_context(|| "database_fn: failed to initialize database")
}
