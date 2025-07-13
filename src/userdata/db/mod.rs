use mlua::{AnyUserData, Lua, Table, UserData, UserDataMethods};
use rusqlite::Connection;
use std::sync::{Arc, Mutex};

use crate::error::{Context, Result};
use crate::userdata::db::types::Conn;

pub(crate) mod execute;
pub(crate) mod query;
pub(crate) mod query_one;
pub(crate) mod query_row;
pub(crate) mod types;

pub(crate) struct LuaDatabase(Conn);

impl UserData for LuaDatabase {
    fn add_methods<'lua, M: UserDataMethods<Self>>(methods: &mut M) {
        methods.add_method_mut("execute", |_, db, args: (String, Option<Table>)| {
            let conn = db.0.clone();
            let (sql, params) = args;
            execute::call(conn, &sql, params)
        });

        methods.add_method_mut("query_row", |lua, db, args: (String, Option<Table>)| {
            let conn = db.0.clone();
            let (sql, params) = args;
            query_row::call(lua, conn, &sql, params)
        });

        methods.add_method_mut("query_one", |lua, db, args: (String, Option<Table>)| {
            let conn = db.0.clone();
            let (sql, params) = args;
            query_one::call(lua, conn, &sql, params)
        });

        methods.add_method_mut("query", |lua, db, args: (String, Option<Table>)| {
            let conn = db.0.clone();
            let (sql, params) = args;
            query::call(lua, conn, &sql, params)
        });
    }
}

/// SQLite function support.
pub(crate) fn create_database_fn(lua: &Lua, path: &str) -> Result<AnyUserData> {
    let db =
        Connection::open(path).with_context(|| format!("Failed to open database at {path}"))?;
    let conn = Arc::new(Mutex::new(db));
    lua.create_userdata(LuaDatabase(conn))
        .with_context(|| "database_fn: failed to initialize database")
}
