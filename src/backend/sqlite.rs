//! SQLite. Set fields are JSON arrays of strings and date fields are ISO-8601 text.

use std::sync::{Arc, Mutex};

use regex::{Regex, RegexBuilder};
use rusqlite::functions::FunctionFlags;
use rusqlite::types::ValueRef;
use rusqlite::{Connection, params_from_iter};

use super::sql::{Cell, Column, Dialect, Param, SqlBackend, SqlRunner};
use crate::collection::Collection;

pub struct SqliteDialect;

/// Whether no member of the JSON array `inner` is missing from the JSON array `outer`.
fn subset(inner: &str, outer: &str) -> String {
    format!(
        "NOT EXISTS (SELECT 1 FROM json_each({inner}) AS m WHERE m.value NOT IN (SELECT value FROM json_each({outer})))"
    )
}

impl Dialect for SqliteDialect {
    fn ident(&self, name: &str) -> String {
        format!("\"{}\"", name.replace('"', "\"\""))
    }

    fn placeholder(&self, _position: usize, _param: &Param) -> String {
        "?".to_string()
    }

    fn regex(&self, column: &Column, placeholder: &str) -> String {
        format!("{} REGEXP {placeholder}", column.sql)
    }

    fn set_has_all(&self, column: &Column, placeholder: &str) -> String {
        subset(placeholder, &column.sql)
    }

    fn set_within(&self, column: &Column, placeholder: &str) -> String {
        subset(&column.sql, placeholder)
    }

    fn set_count(&self, column: &Column) -> String {
        format!("json_array_length({})", column.sql)
    }

    fn encode_set(&self, _column: &Column, members: &[String]) -> String {
        serde_json::Value::from(members).to_string()
    }
}

/// A SQLite connection, shared between threads one statement at a time.
pub struct SqliteRunner {
    connection: Mutex<Connection>,
}

impl SqliteRunner {
    /// Runs a closure with the connection, for work outside the backend such as loading rows.
    pub fn with<T>(&self, work: impl FnOnce(&Connection) -> T) -> T {
        work(&self.connection.lock().unwrap_or_else(|poisoned| poisoned.into_inner()))
    }
}

impl SqlRunner for SqliteRunner {
    fn run(&self, sql: &str, params: &[Param]) -> Result<Vec<Vec<Cell>>, String> {
        let bound = params.iter().map(|param| match param {
            Param::Text(text) => rusqlite::types::Value::Text(text.clone()),
            Param::Int(value) => rusqlite::types::Value::Integer(*value),
            Param::Real(value) => rusqlite::types::Value::Real(*value),
        });
        self.with(|connection| {
            let mut statement = connection.prepare_cached(sql)?;
            let width = statement.column_count();
            let mut rows = statement.query(params_from_iter(bound))?;
            let mut found = Vec::new();
            while let Some(row) = rows.next()? {
                let mut cells = Vec::with_capacity(width);
                for index in 0..width {
                    cells.push(match row.get_ref(index)? {
                        ValueRef::Null => Cell::Null,
                        ValueRef::Integer(value) => Cell::Int(value),
                        ValueRef::Real(value) => Cell::Real(value),
                        ValueRef::Text(text) | ValueRef::Blob(text) => {
                            Cell::Text(String::from_utf8_lossy(text).into_owned())
                        }
                    });
                }
                found.push(cells);
            }
            Ok(found)
        })
        .map_err(|error: rusqlite::Error| error.to_string())
    }
}

pub type SqliteBackend = SqlBackend<SqliteDialect, SqliteRunner>;

/// A backend over a SQLite connection, querying the table named after the collection.
///
/// SQLite has no regular expression engine of its own, so this registers a `regexp`
/// function on the connection. It uses the same linear-time engine as the in-memory
/// backend, so a pattern cannot stall the database.
pub fn sqlite_backend(collection: &Collection, connection: Connection) -> rusqlite::Result<SqliteBackend> {
    let flags = FunctionFlags::SQLITE_UTF8 | FunctionFlags::SQLITE_DETERMINISTIC;
    connection.create_scalar_function("regexp", 2, flags, |context| {
        type BoxError = Box<dyn std::error::Error + Send + Sync + 'static>;
        let pattern: Arc<Regex> = context.get_or_create_aux(0, |source| -> Result<_, BoxError> {
            Ok(RegexBuilder::new(source.as_str()?).case_insensitive(true).build()?)
        })?;
        Ok(match context.get_raw(1) {
            ValueRef::Text(text) => pattern.is_match(&String::from_utf8_lossy(text)),
            _ => false,
        })
    })?;
    let runner = SqliteRunner { connection: Mutex::new(connection) };
    Ok(SqlBackend::new(collection, SqliteDialect, runner))
}
