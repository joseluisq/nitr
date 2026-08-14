use rusqlite::{params_from_iter, Connection};

use crate::db::types::{read_row, SqlRow, SqlValue};

/// Runs a query and returns all result rows.
pub(crate) fn call(
    conn: &Connection,
    sql: &str,
    params: &[SqlValue],
) -> Result<Vec<SqlRow>, rusqlite::Error> {
    let mut stmt = conn.prepare_cached(sql)?;
    let columns = stmt
        .column_names()
        .iter()
        .map(|s| s.to_string())
        .collect::<Vec<_>>();

    let mut rows = stmt.query(params_from_iter(params))?;
    let mut out = vec![];
    while let Some(row) = rows.next()? {
        out.push(read_row(&columns, row)?);
    }
    Ok(out)
}
