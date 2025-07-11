use mlua::{
    AnyUserData, ExternalResult, Lua, LuaSerdeExt, Table, UserData, UserDataMethods, Value,
};
use rusqlite::{params_from_iter, Connection, ToSql};
use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
};

use crate::error::{Context, Result};

trait SQLiteParams {
    fn get_params(&self) -> Result<Vec<Box<dyn ToSql>>>;
}

pub(crate) struct LuaDatabase(Arc<Mutex<Connection>>);

impl UserData for LuaDatabase {
    fn add_methods<'lua, M: UserDataMethods<Self>>(methods: &mut M) {
        methods.add_method_mut("execute", |_, db, args: (String, Option<Table>)| {
            let conn = db.0.clone();
            let conn = conn
                .lock()
                .map_err(|_| {
                    mlua::Error::RuntimeError("Failed to lock database connection".to_string())
                })
                .into_lua_err()?;

            let sql = args.0;
            let params = args.1;
            let params = params_from_iter(
                params
                    .map_or(Ok(vec![]), |t| t.get_params())
                    .with_context(|| "Failed to convert Lua table to SQL parameters")?,
            );
            match conn.execute(sql.as_str(), params) {
                Ok(affected) => Ok(Value::Integer(affected as i64)),
                Err(err) => Err(mlua::Error::RuntimeError(err.to_string())),
            }
        });

        methods.add_method_mut("query_row", |lua, db, args: (String, Option<Table>)| {
            let conn = db.0.clone();
            let conn = conn
                .lock()
                .map_err(|_| {
                    mlua::Error::RuntimeError("Failed to lock database connection".to_string())
                })
                .into_lua_err()?;

            let sql = args.0;
            let params = args.1;
            let params = params_from_iter(
                params
                    .map_or(Ok(vec![]), |t| t.get_params())
                    .with_context(|| "Failed to convert Lua table to SQL parameters")?,
            );

            let mut stmt = conn
                .prepare(sql.as_str())
                .with_context(|| format!("Failed to prepare SQL statement: {sql}"))?;
            let columns = stmt
                .column_names()
                .iter()
                .map(|s| s.to_string())
                .collect::<Vec<_>>();

            let row = stmt
                .query_row(params, |sqlrow| {
                    let mut datarow = BTreeMap::new();
                    for (i, column) in columns.iter().enumerate() {
                        let record = match sqlrow.get_ref(i).with_context(|| {
                            format!("Unable to get value for column '{column}' ({i})")
                        }) {
                            Ok(v) => v,
                            Err(_) => return Err(rusqlite::Error::InvalidColumnIndex(i))?,
                        };
                        let name = column.to_string();
                        let value = match record {
                            rusqlite::types::ValueRef::Null => Value::Nil,
                            rusqlite::types::ValueRef::Integer(i) => Value::Integer(i),
                            rusqlite::types::ValueRef::Real(f) => Value::Number(f),
                            rusqlite::types::ValueRef::Text(s) => match lua
                                .to_value(&String::from_utf8_lossy(s))
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
                            },
                            rusqlite::types::ValueRef::Blob(b) => match lua
                                .to_value(&String::from_utf8_lossy(b))
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
                .with_context(|| format!("Failed to run SQL statement: {sql}"))?;

            lua.to_value(&row)
        });

        methods.add_method_mut("query_one", |lua, db, args: (String, Option<Table>)| {
            let conn = db.0.clone();
            let conn = conn
                .lock()
                .map_err(|_| {
                    mlua::Error::RuntimeError("Failed to lock database connection".to_string())
                })
                .into_lua_err()?;

            let sql = args.0;
            let params = args.1;
            let params = params_from_iter(
                params
                    .map_or(Ok(vec![]), |t| t.get_params())
                    .with_context(|| "Failed to convert Lua table to SQL parameters")?,
            );

            let mut stmt = conn
                .prepare(sql.as_str())
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
                        let record = match sqlrow.get_ref(i).with_context(|| {
                            format!("Unable to get value for column '{column}' ({i})")
                        }) {
                            Ok(v) => v,
                            Err(_) => return Err(rusqlite::Error::InvalidColumnIndex(i))?,
                        };
                        let name = column.to_string();
                        let value = match record {
                            rusqlite::types::ValueRef::Null => Value::Nil,
                            rusqlite::types::ValueRef::Integer(i) => Value::Integer(i),
                            rusqlite::types::ValueRef::Real(f) => Value::Number(f),
                            rusqlite::types::ValueRef::Text(s) => match lua
                                .to_value(&String::from_utf8_lossy(s))
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
                            },
                            rusqlite::types::ValueRef::Blob(b) => match lua
                                .to_value(&String::from_utf8_lossy(b))
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
                .with_context(|| format!("Failed to run SQL statement: `{sql}`, query result expects exactly one row."))?;

            lua.to_value(&row)
        });

        methods.add_method_mut("query", |lua, db, args: (String, Option<Table>)| {
            let conn = db.0.clone();
            let conn = conn
                .lock()
                .map_err(|_| {
                    mlua::Error::RuntimeError("Failed to lock database connection".to_string())
                })
                .into_lua_err()?;

            let sql = args.0;
            let params = args.1;
            let params = params_from_iter(
                params
                    .map_or(Ok(vec![]), |t| t.get_params())
                    .with_context(|| "Failed to convert Lua table to SQL parameters")?,
            );

            let mut stmt = conn
                .prepare(sql.as_str())
                .with_context(|| format!("Failed to prepare SQL statement: {}", sql))?;
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
                        let table_row = lua.create_table()?;
                        for (i, column) in columns.iter().enumerate() {
                            let record = row.get_ref(i).with_context(|| {
                                format!("Failed to get value for column '{}'", column)
                            })?;
                            let name = column.to_string();
                            let value = match record {
                                rusqlite::types::ValueRef::Null => Value::Nil,
                                rusqlite::types::ValueRef::Integer(i) => Value::Integer(i),
                                rusqlite::types::ValueRef::Real(f) => Value::Number(f),
                                rusqlite::types::ValueRef::Text(s) => lua
                                    .to_value(&String::from_utf8_lossy(s))
                                    .with_context(|| "Failed to convert text to Lua value")?,
                                rusqlite::types::ValueRef::Blob(b) => lua
                                    .to_value(&String::from_utf8_lossy(b))
                                    .with_context(|| "Failed to convert blob to Lua value")?,
                            };
                            table_row.set(name, value).with_context(|| {
                                format!("Failed to set value for column '{}'", column)
                            })?;
                        }
                        table_result
                            .raw_set(table_result.raw_len() + 1, table_row)
                            .with_context(|| "Failed to set row in Lua result table")?;
                    }
                    lua.to_value(&table_result)
                }
                Err(err) => Err(mlua::Error::RuntimeError(err.to_string())),
            }
        });
    }
}

/// SQLite function support.
pub(crate) fn create_database_fn(lua: &Lua, path: &str) -> Result<AnyUserData> {
    let db =
        Connection::open(path).with_context(|| format!("Failed to open database at {}", path))?;
    let conn = Arc::new(Mutex::new(db));
    lua.create_userdata(LuaDatabase(conn))
        .with_context(|| "database_fn: failed to initialize database")
}

impl SQLiteParams for Table {
    fn get_params(&self) -> Result<Vec<Box<dyn ToSql>>> {
        let mut params: Vec<Box<dyn ToSql>> = vec![];
        for pair in self.pairs::<Value, Value>() {
            let (_, v) = pair.with_context(|| "Failed to get pair from Lua table")?;
            match v {
                Value::Nil => params.push(Box::new(Option::<u64>::None)),
                Value::Integer(i) => params.push(Box::new(i)),
                Value::Number(n) => params.push(Box::new(n)),
                Value::String(s) => params.push(Box::new(s.to_string_lossy())),
                Value::Boolean(b) => params.push(Box::new(b)),
                _ => {
                    // Unsupported value type for SQL parameter
                    continue;
                }
            }
        }
        Ok(params)
    }
}
