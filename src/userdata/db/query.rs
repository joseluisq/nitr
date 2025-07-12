use mlua::{Error, ExternalResult, Lua, LuaSerdeExt, Table, Value};
use rusqlite::params_from_iter;

use crate::error::{Context, Result};
use crate::userdata::db::types::{Conn, SqlParamsExt};

pub(crate) fn call(
    lua: &Lua,
    conn: Conn,
    sql: &str,
    params: Option<Table>,
) -> Result<Value, Error> {
    let conn = conn
        .lock()
        .map_err(|_| Error::RuntimeError("Failed to lock database connection".to_string()))
        .into_lua_err()?;

    let params = params_from_iter(
        params
            .map_or(Ok(vec![]), |t| t.get_params())
            .with_context(|| "Failed to convert Lua table to SQL parameters")?,
    );

    let mut stmt = conn
        .prepare(sql)
        .with_context(|| format!("Failed to prepare SQL statement: {sql}"))?;
    let columns = stmt
        .column_names()
        .iter()
        .map(|s| s.to_string())
        .collect::<Vec<_>>();

    let rows = stmt.query(params);
    match rows {
        Ok(mut rows) => {
            let table_result = lua
                .create_table()
                .with_context(|| "Failed to create Lua result table")?;

            while let Some(row) = rows
                .next()
                .with_context(|| "Failed to fetch row from statement")?
            {
                let table_row = lua
                    .create_table()
                    .with_context(|| "Failed to create Lua table for row")?;

                for (i, column) in columns.iter().enumerate() {
                    let record = row.get_ref(i).with_context(|| {
                        format!("Unable to get value for column '{column}' ({i})")
                    })?;
                    let value = match record {
                        rusqlite::types::ValueRef::Null => Value::Nil,
                        rusqlite::types::ValueRef::Integer(i) => Value::Integer(i),
                        rusqlite::types::ValueRef::Real(f) => Value::Number(f),
                        rusqlite::types::ValueRef::Text(s) => {
                            let s = lua
                                .create_string(s)
                                .with_context(|| "Failed to convert text to Lua value")?;
                            Value::String(s)
                        }
                        rusqlite::types::ValueRef::Blob(b) => lua
                            .to_value(b)
                            .with_context(|| "Failed to convert blob to Lua value")?,
                    };
                    table_row
                        .set(column.to_string(), value)
                        .with_context(|| format!("Failed to set value for column '{column}'"))?;
                }
                table_result
                    .raw_set(table_result.raw_len() + 1, table_row)
                    .with_context(|| "Failed to set row in Lua result table")?;
            }
            lua.to_value(&table_result)
        }
        Err(err) => Err(mlua::Error::RuntimeError(err.to_string())),
    }
}
