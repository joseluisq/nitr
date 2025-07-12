use mlua::{Error, ExternalResult, Lua, LuaSerdeExt, Table, Value};
use rusqlite::params_from_iter;
use std::collections::BTreeMap;

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

    let row = stmt
        .query_one(params, |sqlrow| {
            let mut datarow = BTreeMap::new();
            for (i, column) in columns.iter().enumerate() {
                let record = match sqlrow
                    .get_ref(i)
                    .with_context(|| format!("Unable to get value for column '{column}' ({i})"))
                {
                    Ok(v) => v,
                    Err(_) => return Err(rusqlite::Error::InvalidColumnIndex(i))?,
                };
                let name = column.to_string();
                let value = match record {
                    rusqlite::types::ValueRef::Null => Value::Nil,
                    rusqlite::types::ValueRef::Integer(i) => Value::Integer(i),
                    rusqlite::types::ValueRef::Real(f) => Value::Number(f),
                    rusqlite::types::ValueRef::Text(s) => {
                        let s = match lua
                            .create_string(s)
                            .with_context(|| "Failed to convert text to Lua value")
                        {
                            Ok(v) => v,
                            Err(_) => {
                                return Err(rusqlite::Error::InvalidColumnType(
                                    i,
                                    "Text".to_string(),
                                    rusqlite::types::Type::Text,
                                ))
                            }
                        };
                        Value::String(s)
                    }
                    rusqlite::types::ValueRef::Blob(b) => match lua
                        .to_value(b)
                        .with_context(|| "Failed to convert blob to Lua value")
                    {
                        Ok(v) => v,
                        Err(_) => {
                            return Err(rusqlite::Error::InvalidColumnType(
                                i,
                                "Blob".to_string(),
                                rusqlite::types::Type::Blob,
                            ))
                        }
                    },
                };
                datarow.insert(name, value);
            }
            Ok(datarow)
        })
        .with_context(|| {
            format!("Failed to run SQL statement: `{sql}`, query result expects exactly one row.")
        })?;

    lua.to_value(&row)
}
