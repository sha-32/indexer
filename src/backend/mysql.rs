//! MySQL and MariaDB.
//!
//! Set fields are JSON arrays of strings. Ordinal and keyword fields may be text or
//! `ENUM` columns.
//!
//! Two things follow the server and not this crate. Text is compared under the
//! column's collation, which by default ignores letter case, so a keyword matches in
//! any case and text sorts without regard to it. And regular expressions are checked
//! against this crate's syntax when a query is parsed but matched by the server's
//! engine, which reads a few constructs differently.

use std::collections::HashMap;
use std::error::Error as StdError;
use std::sync::Mutex;

use mysql::prelude::Queryable;
use mysql::{Conn, Params, Value};

use super::sql::{Cell, Column, Dialect, Param, SqlBackend, SqlRunner};
use crate::collection::{Collection, DefinitionError};
use crate::schema::FieldKind;

/// How a date field is stored.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum DateStorage {
    /// A `DATETIME` or `TIMESTAMP` holding UTC, ideally with six fractional digits.
    /// Rows come back as UTC ISO timestamps with all six.
    #[default]
    Datetime,
    /// Calendar days. Literals and `now` are compared by their UTC day, and rows come
    /// back as `YYYY-MM-DD`.
    Date,
    /// ISO-8601 UTC strings, compared as text.
    Text,
}

/// How a collection's fields are stored in MySQL or MariaDB.
#[derive(Clone, Debug, Default)]
pub struct MySqlOptions {
    dates: DateStorage,
    date_columns: HashMap<String, DateStorage>,
}

impl MySqlOptions {
    /// The storage of date fields that are not given their own.
    pub fn dates(mut self, storage: DateStorage) -> Self {
        self.dates = storage;
        self
    }

    /// The storage of one date field, by field name.
    pub fn date_column(mut self, field: impl Into<String>, storage: DateStorage) -> Self {
        self.date_columns.insert(field.into(), storage);
        self
    }

    /// Checks that every field given its own storage exists and is a date field.
    fn check(&self, collection: &Collection) -> Result<(), DefinitionError> {
        for field in self.date_columns.keys() {
            if !collection.fields().any(|(name, declared)| name == field && declared.kind == FieldKind::Date)
            {
                return Err(DefinitionError(format!(
                    "Collection \"{}\": \"{field}\" is not a date field",
                    collection.name()
                )));
            }
        }
        Ok(())
    }
}

pub struct MySqlDialect {
    options: MySqlOptions,
}

impl MySqlDialect {
    pub fn new(options: MySqlOptions) -> MySqlDialect {
        MySqlDialect { options }
    }

    fn date_of(&self, column: &Column) -> DateStorage {
        *self.options.date_columns.get(column.field).unwrap_or(&self.options.dates)
    }
}

impl Dialect for MySqlDialect {
    fn ident(&self, name: &str) -> String {
        format!("`{}`", name.replace('`', "``"))
    }

    fn placeholder(&self, _position: usize, _param: &Param) -> String {
        "?".to_string()
    }

    /// The inline flag makes the match ignore case on both servers, whatever the
    /// column's collation.
    fn regex(&self, column: &Column, placeholder: &str) -> String {
        format!("{} REGEXP CONCAT('(?i)', {placeholder})", column.sql)
    }

    /// Neither server reads the `T` and `Z` of an ISO timestamp, so the instant is sent
    /// in their own form. The session is kept in UTC, so it means what it says.
    fn date(&self, column: &Column, placeholder: &str, value: &str) -> (String, String) {
        match self.date_of(column) {
            DateStorage::Datetime => {
                let instant = value.trim_end_matches('Z').replacen('T', " ", 1);
                (format!("CAST({placeholder} AS DATETIME(6))"), instant)
            }
            DateStorage::Date => (format!("CAST({placeholder} AS DATE)"), value.chars().take(10).collect()),
            DateStorage::Text => (placeholder.to_string(), value.to_string()),
        }
    }

    /// A date and a datetime compare as instants natively. ISO text has to be read as
    /// a datetime first.
    fn operand(&self, column: &Column, other: &Column) -> String {
        let mine = self.date_of(column);
        if column.kind != FieldKind::Date || mine == self.date_of(other) || mine != DateStorage::Text {
            return column.sql.clone();
        }
        format!("CAST(REPLACE(REPLACE({}, 'T', ' '), 'Z', '') AS DATETIME(6))", column.sql)
    }

    fn set_has_all(&self, column: &Column, placeholder: &str) -> String {
        format!("JSON_CONTAINS({}, {placeholder})", column.sql)
    }

    fn set_within(&self, column: &Column, placeholder: &str) -> String {
        format!("JSON_CONTAINS({placeholder}, {})", column.sql)
    }

    fn set_count(&self, column: &Column) -> String {
        format!("JSON_LENGTH({})", column.sql)
    }

    fn encode_set(&self, _column: &Column, members: &[String]) -> String {
        serde_json::Value::from(members).to_string()
    }

    /// Dates are read as text, which keeps the microseconds a cursor needs to name a
    /// row's position exactly.
    fn select(&self, column: &Column) -> String {
        let col = &column.sql;
        match (column.kind, self.date_of(column)) {
            (FieldKind::Date, DateStorage::Datetime) => {
                format!("DATE_FORMAT({col}, '%Y-%m-%dT%H:%i:%s.%fZ')")
            }
            (FieldKind::Date, DateStorage::Date) => format!("DATE_FORMAT({col}, '%Y-%m-%d')"),
            _ => col.clone(),
        }
    }
}

/// A MySQL or MariaDB connection, shared between threads one statement at a time.
pub struct MySqlRunner {
    connection: Mutex<Conn>,
}

impl MySqlRunner {
    /// Runs a closure with the connection, for work outside the backend such as loading rows.
    pub fn with<T>(&self, work: impl FnOnce(&mut Conn) -> T) -> T {
        work(&mut self.connection.lock().unwrap_or_else(|poisoned| poisoned.into_inner()))
    }
}

impl SqlRunner for MySqlRunner {
    fn run(&self, sql: &str, params: &[Param]) -> Result<Vec<Vec<Cell>>, String> {
        let bound: Vec<Value> = params
            .iter()
            .map(|param| match param {
                Param::Text(text) => Value::Bytes(text.clone().into_bytes()),
                Param::Int(value) => Value::Int(*value),
                Param::Real(value) => Value::Double(*value),
            })
            .collect();
        let bound = if bound.is_empty() { Params::Empty } else { Params::Positional(bound) };

        self.with(|connection| {
            let rows: Vec<mysql::Row> = connection.exec(sql, bound)?;
            let mut found = Vec::with_capacity(rows.len());
            for row in rows {
                let mut cells = Vec::with_capacity(row.len());
                for value in row.unwrap() {
                    cells.push(match value {
                        Value::NULL => Cell::Null,
                        Value::Int(value) => Cell::Int(value),
                        Value::UInt(value) => Cell::Int(i64::try_from(value).unwrap_or(i64::MAX)),
                        Value::Float(value) => Cell::Real(f64::from(value)),
                        Value::Double(value) => Cell::Real(value),
                        Value::Bytes(bytes) => Cell::Text(String::from_utf8_lossy(&bytes).into_owned()),
                        // Date columns are selected through DATE_FORMAT, so a native
                        // temporal value means the collection calls something else a date.
                        other => Cell::Text(other.as_sql(true).trim_matches('\'').to_string()),
                    });
                }
                found.push(cells);
            }
            Ok(found)
        })
        .map_err(|error: mysql::Error| error.to_string())
    }
}

pub type MySqlBackend = SqlBackend<MySqlDialect, MySqlRunner>;

/// A backend over a MySQL or MariaDB connection, querying the table named after the
/// collection.
///
/// The session time zone is set to UTC, so that date literals and `TIMESTAMP` columns
/// mean the same instant whatever the server's own zone is.
pub fn mysql_backend(
    collection: &Collection,
    mut connection: Conn,
    options: MySqlOptions,
) -> Result<MySqlBackend, Box<dyn StdError + Send + Sync>> {
    options.check(collection)?;
    connection.query_drop("SET time_zone = '+00:00'")?;
    let runner = MySqlRunner { connection: Mutex::new(connection) };
    Ok(SqlBackend::new(collection, MySqlDialect::new(options), runner))
}
