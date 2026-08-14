//! The `conn` builtin: SQLite statements and transactions. Blocking
//! rusqlite calls run on the blocking thread pool; each Lua state owns its
//! own connection, and requests are serialized per state, so a transaction
//! never interleaves with other statements.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use mlua::{AnyUserData, Function, Lua, Table, UserData, UserDataMethods, Value};
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

/// One (possibly nested) transaction scope handed to the Lua callback of
/// `conn:transaction(fn)` / `tx:transaction(fn)`.
pub(crate) struct LuaTransaction {
    conn: Conn,
    /// Names nested savepoints uniquely within this scope.
    savepoints: AtomicUsize,
}

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

/// Executes a control statement (`BEGIN`, `COMMIT`, `SAVEPOINT ...`).
async fn exec_batch(conn: Conn, sql: String) -> mlua::Result<()> {
    run_blocking(conn, sql, Vec::new(), |conn, sql, _| {
        conn.execute_batch(sql)
    })
    .await
}

/// Registers `execute`/`query`/`query_row`/`query_one` on a userdata type
/// that exposes a connection — shared between `conn` and transactions.
fn add_stmt_methods<T, M>(methods: &mut M, conn_of: fn(&T) -> Conn)
where
    T: UserData + 'static,
    M: UserDataMethods<T>,
{
    // The connection is cloned out before each async block so no
    // userdata borrow lives across an await point.
    methods.add_async_method("execute", move |_, this, args: (String, Option<Table>)| {
        let conn = conn_of(&this);
        async move {
            let (sql, params) = args;
            let params = params_from_table(params.as_ref())?;
            let affected = run_blocking(conn, sql, params, execute::call).await?;
            Ok(affected)
        }
    });

    methods.add_async_method(
        "query_row",
        move |lua, this, args: (String, Option<Table>)| {
            let conn = conn_of(&this);
            async move {
                let (sql, params) = args;
                let params = params_from_table(params.as_ref())?;
                let row = run_blocking(conn, sql, params, query_row::call).await?;
                row_to_lua(&lua, row)
            }
        },
    );

    methods.add_async_method(
        "query_one",
        move |lua, this, args: (String, Option<Table>)| {
            let conn = conn_of(&this);
            async move {
                let (sql, params) = args;
                let params = params_from_table(params.as_ref())?;
                let row = run_blocking(conn, sql, params, query_one::call).await?;
                row_to_lua(&lua, row)
            }
        },
    );

    methods.add_async_method("query", move |lua, this, args: (String, Option<Table>)| {
        let conn = conn_of(&this);
        async move {
            let (sql, params) = args;
            let params = params_from_table(params.as_ref())?;
            let rows = run_blocking(conn, sql, params, query::call).await?;
            let table = lua.create_table()?;
            for (i, row) in rows.into_iter().enumerate() {
                table.raw_set(i + 1, row_to_lua(&lua, row)?)?;
            }
            Ok(table)
        }
    });
}

/// Runs the transaction body between `begin` and `commit`/`rollback`,
/// passing a fresh [`LuaTransaction`] scope and re-raising body errors
/// after rolling back.
async fn run_transaction(
    lua: &Lua,
    conn: Conn,
    f: Function,
    begin: String,
    commit: String,
    rollback: String,
) -> mlua::Result<Value> {
    exec_batch(conn.clone(), begin).await?;
    let scope = lua.create_userdata(LuaTransaction {
        conn: conn.clone(),
        savepoints: AtomicUsize::new(0),
    })?;
    match f.call_async::<Value>(&scope).await {
        Ok(value) => {
            exec_batch(conn, commit).await?;
            Ok(value)
        }
        Err(err) => {
            if let Err(rollback_err) = exec_batch(conn, rollback).await {
                tracing::error!("transaction rollback failed: {rollback_err}");
            }
            Err(err)
        }
    }
}

impl UserData for LuaDatabase {
    fn add_methods<M: UserDataMethods<Self>>(methods: &mut M) {
        add_stmt_methods(methods, |db: &LuaDatabase| db.0.clone());

        // conn:transaction(function(tx) ... end): commits when the
        // function returns, rolls back (and re-raises) when it errors.
        // Use `tx`, not `conn`, for statements inside the body.
        methods.add_async_method("transaction", |lua, db, f: Function| {
            let conn = db.0.clone();
            async move {
                run_transaction(
                    &lua,
                    conn,
                    f,
                    "BEGIN".into(),
                    "COMMIT".into(),
                    "ROLLBACK".into(),
                )
                .await
            }
        });
    }
}

impl UserData for LuaTransaction {
    fn add_methods<M: UserDataMethods<Self>>(methods: &mut M) {
        add_stmt_methods(methods, |tx: &LuaTransaction| tx.conn.clone());

        // Nested transactions become savepoints: rolling back the inner
        // scope keeps the outer transaction alive.
        methods.add_async_method("transaction", |lua, tx, f: Function| {
            let conn = tx.conn.clone();
            let n = tx.savepoints.fetch_add(1, Ordering::Relaxed);
            async move {
                let name = format!("nitr_sp_{n}");
                run_transaction(
                    &lua,
                    conn,
                    f,
                    format!("SAVEPOINT {name}"),
                    format!("RELEASE {name}"),
                    format!("ROLLBACK TO {name}; RELEASE {name}"),
                )
                .await
            }
        });
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
