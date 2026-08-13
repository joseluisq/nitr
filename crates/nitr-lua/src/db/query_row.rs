use rusqlite::{params_from_iter, Connection};

use crate::db::types::{read_row, SqlRow, SqlValue};

/// Runs a query expected to return at least one row and returns the first.
pub(crate) fn call(
    conn: &Connection,
    sql: &str,
    params: &[SqlValue],
) -> Result<SqlRow, rusqlite::Error> {
    let mut stmt = conn.prepare_cached(sql)?;
    let columns = stmt
        .column_names()
        .iter()
        .map(|s| s.to_string())
        .collect::<Vec<_>>();

    stmt.query_row(params_from_iter(params), |row| read_row(&columns, row))
}
