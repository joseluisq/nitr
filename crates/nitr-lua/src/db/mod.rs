use std::sync::{Arc, Mutex};
use std::time::Duration;

use mlua::{AnyUserData, Lua, Table, UserData, UserDataMethods};
use rusqlite::Connection;

use crate::db::types::{params_from_table, row_to_lua, Conn, SqlValue};
use nitr_core::{Error, Result};

pub(crate) mod execute;
pub(crate) mod query;
pub(crate) mod query_one;
pub(crate) mod query_row;
pub(crate) mod types;

/// How long a statement waits on a locked database before failing.
const BUSY_TIMEOUT: Duration = Duration::from_secs(5);

pub(crate) struct LuaDatabase(Conn);

/// Runs a blocking database operation on the blocking thread pool so it
/// stalls a blocking-pool thread instead of an async worker. Only plain
/// `Send` data crosses the boundary — never a Lua handle.
async fn run_blocking<T, F>(conn: Conn, sql: String, params: Vec<SqlValue>, f: F) -> mlua::Result<T>
where
    T: Send + 'static,
    F: FnOnce(&Connection, &str, &[SqlValue]) -> Result<T, rusqlite::Error> + Send + 'static,
{
    tokio::task::spawn_blocking(move || {
        let conn = conn.lock().map_err(|_| {
            mlua::Error::RuntimeError("failed to lock the database connection".into())
        })?;
        f(&conn, &sql, &params).map_err(|err| {
            mlua::Error::RuntimeError(format!("SQL statement `{sql}` failed: {err}"))
        })
    })
    .await
    .map_err(mlua::Error::external)?
}

impl UserData for LuaDatabase {
    fn add_methods<M: UserDataMethods<Self>>(methods: &mut M) {
        methods.add_async_method(
            "execute",
            |_, db, args: (String, Option<Table>)| async move {
                let (sql, params) = args;
                let params = params_from_table(params.as_ref())?;
                let affected = run_blocking(db.0.clone(), sql, params, execute::call).await?;
                Ok(affected)
            },
        );

        methods.add_async_method(
            "query_row",
            |lua, db, args: (String, Option<Table>)| async move {
                let (sql, params) = args;
                let params = params_from_table(params.as_ref())?;
                let row = run_blocking(db.0.clone(), sql, params, query_row::call).await?;
                row_to_lua(&lua, row)
            },
        );

        methods.add_async_method(
            "query_one",
            |lua, db, args: (String, Option<Table>)| async move {
                let (sql, params) = args;
                let params = params_from_table(params.as_ref())?;
                let row = run_blocking(db.0.clone(), sql, params, query_one::call).await?;
                row_to_lua(&lua, row)
            },
        );

        methods.add_async_method(
            "query",
            |lua, db, args: (String, Option<Table>)| async move {
                let (sql, params) = args;
                let params = params_from_table(params.as_ref())?;
                let rows = run_blocking(db.0.clone(), sql, params, query::call).await?;
                let table = lua.create_table()?;
                for (i, row) in rows.into_iter().enumerate() {
                    table.raw_set(i + 1, row_to_lua(&lua, row)?)?;
                }
                Ok(table)
            },
        );
    }
}

/// SQLite function support.
pub(crate) fn create_database_fn(lua: &Lua, path: &std::path::Path) -> Result<AnyUserData> {
    let db = Connection::open(path).map_err(|err| {
        Error::Config(format!(
            "failed to open database at {}: {err}",
            path.display()
        ))
    })?;
    db.busy_timeout(BUSY_TIMEOUT).map_err(|err| {
        Error::Config(format!(
            "failed to set the busy timeout on database {}: {err}",
            path.display()
        ))
    })?;
    let conn = Arc::new(Mutex::new(db));
    let value = lua.create_userdata(LuaDatabase(conn))?;
    Ok(value)
}
