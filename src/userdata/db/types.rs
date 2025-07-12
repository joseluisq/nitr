use mlua::{Table, Value};
use rusqlite::{Connection, ToSql};
use std::sync::{Arc, Mutex};

use crate::error::{Context, Result};

pub(crate) type Conn = Arc<Mutex<Connection>>;

pub(crate) trait SqlParamsExt {
    fn get_params(&self) -> Result<Vec<Box<dyn ToSql>>>;
}

impl SqlParamsExt for Table {
    fn get_params(&self) -> Result<Vec<Box<dyn ToSql>>> {
        let mut params: Vec<Box<dyn ToSql>> = vec![];
        for pair in self.pairs::<Value, Value>() {
            let (_, v) = pair.with_context(|| "Failed to get pair from Lua table")?;
            match v {
                Value::Nil => params.push(Box::new(Option::<u64>::None)),
                Value::Integer(i) => params.push(Box::new(i)),
                Value::Number(n) => params.push(Box::new(n)),
                Value::String(s) => params.push(Box::new(s.as_bytes().to_vec())),
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
