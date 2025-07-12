use mlua::{Error, ExternalResult, Table, Value};
use rusqlite::params_from_iter;

use crate::error::{Context, Result};
use crate::userdata::db::types::{Conn, SqlParamsExt};

pub(crate) fn call(conn: Conn, sql: &str, params: Option<Table>) -> Result<Value, Error> {
    let conn = conn
        .lock()
        .map_err(|_| mlua::Error::RuntimeError("Failed to lock database connection".to_string()))
        .into_lua_err()?;

    let params = params_from_iter(
        params
            .map_or(Ok(vec![]), |t| t.get_params())
            .with_context(|| "Failed to convert Lua table to SQL parameters")?,
    );
    match conn
        .execute(sql, params)
        .with_context(|| format!("Failed to execute SQL statement: {sql}"))
    {
        Ok(affected) => Ok(Value::Integer(affected as i64)),
        Err(err) => Err(mlua::Error::RuntimeError(err.to_string())),
    }
}
