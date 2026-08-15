use rusqlite::{Connection, params_from_iter};

use crate::db::types::SqlValue;

/// Executes a statement and returns the number of affected rows.
pub(crate) fn call(
    conn: &Connection,
    sql: &str,
    params: &[SqlValue],
) -> Result<usize, rusqlite::Error> {
    let mut stmt = conn.prepare_cached(sql)?;
    stmt.execute(params_from_iter(params))
}
