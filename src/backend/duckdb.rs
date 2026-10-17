//! DuckDB.
//!
//! Set fields are `VARCHAR[]` lists. Ordinal and keyword fields may be text or `ENUM`
//! columns. Regular expressions are matched by DuckDB's own linear-time engine.
//!
//! DuckDB reads Parquet, CSV and JSON files as tables. To search one, create a view
//! over it under the collection's name and point the backend at the connection:
//! `CREATE VIEW tickets AS SELECT * FROM read_parquet('tickets.parquet')`.

use std::collections::HashMap;
use std::sync::Mutex;

use duckdb::types::Value;
use duckdb::{Connection, params_from_iter};

use super::sql::{Cell, Column, Dialect, Param, SqlBackend, SqlRunner};
use crate::collection::{Collection, DefinitionError};
use crate::schema::FieldKind;

/// How a date field is stored.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum DateStorage {
    /// A `TIMESTAMP` holding UTC. Rows come back as UTC ISO timestamps with all six
    /// fractional digits.
    #[default]
    Timestamp,
    /// A `TIMESTAMPTZ`. A date literal means UTC midnight, whatever the session time
    /// zone, and rows come back as for `Timestamp`.
    Timestamptz,
    /// Calendar days. Literals and `now` are compared by their UTC day, and rows come
    /// back as `YYYY-MM-DD`.
    Date,
    /// ISO-8601 UTC strings, compared as text.
    Text,
}

/// How a collection's fields are stored in DuckDB.
#[derive(Clone, Debug, Default)]
pub struct DuckDbOptions {
    dates: DateStorage,
    date_columns: HashMap<String, DateStorage>,
}

impl DuckDbOptions {
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

pub struct DuckDbDialect {
    options: DuckDbOptions,
}

/// An ISO instant in the form DuckDB reads without a time zone: `2026-01-02 09:30:00`.
fn naive(iso: &str) -> String {
    iso.trim_end_matches('Z').replacen('T', " ", 1)
}

const ISO_MICROS: &str = "'%Y-%m-%dT%H:%M:%S.%fZ'";

impl DuckDbDialect {
    pub fn new(options: DuckDbOptions) -> DuckDbDialect {
        DuckDbDialect { options }
    }

    fn date_of(&self, column: &Column) -> DateStorage {
        *self.options.date_columns.get(column.field).unwrap_or(&self.options.dates)
    }

    /// A date column as a `TIMESTAMP` in UTC, whatever it is stored as. A
    /// `TIMESTAMPTZ` goes by way of its epoch so the session time zone has no say.
    fn as_utc(&self, column: &Column) -> String {
        let col = &column.sql;
        match self.date_of(column) {
            DateStorage::Timestamp => col.clone(),
            DateStorage::Timestamptz => format!("make_timestamp(epoch_us({col}))"),
            DateStorage::Date => format!("CAST({col} AS TIMESTAMP)"),
            DateStorage::Text => format!("CAST(replace(replace({col}, 'T', ' '), 'Z', '') AS TIMESTAMP)"),
        }
    }
}

impl Dialect for DuckDbDialect {
    fn ident(&self, name: &str) -> String {
        format!("\"{}\"", name.replace('"', "\"\""))
    }

    fn placeholder(&self, _position: usize, _param: &Param) -> String {
        "?".to_string()
    }

    fn regex(&self, column: &Column, placeholder: &str) -> String {
        format!("regexp_matches({}, {placeholder}, 'i')", column.sql)
    }

    fn date(&self, column: &Column, placeholder: &str, value: &str) -> (String, String) {
        match self.date_of(column) {
            DateStorage::Timestamp => (format!("CAST({placeholder} AS TIMESTAMP)"), naive(value)),
            DateStorage::Timestamptz => {
                let instant = if value.len() == 10 { format!("{value} 00:00:00") } else { naive(value) };
                (format!("CAST({placeholder} AS TIMESTAMPTZ)"), format!("{instant}+00"))
            }
            DateStorage::Date => (format!("CAST({placeholder} AS DATE)"), value.chars().take(10).collect()),
            DateStorage::Text => (placeholder.to_string(), value.to_string()),
        }
    }

    /// Two date fields stored differently are both read as UTC timestamps.
    fn operand(&self, column: &Column, other: &Column) -> String {
        if column.kind != FieldKind::Date || self.date_of(column) == self.date_of(other) {
            return column.sql.clone();
        }
        self.as_utc(column)
    }

    fn set_has_all(&self, column: &Column, placeholder: &str) -> String {
        format!("list_has_all({}, CAST({placeholder} AS VARCHAR[]))", column.sql)
    }

    fn set_within(&self, column: &Column, placeholder: &str) -> String {
        format!("list_has_all(CAST({placeholder} AS VARCHAR[]), {})", column.sql)
    }

    fn set_count(&self, column: &Column) -> String {
        format!("len({})", column.sql)
    }

    /// A JSON array is also how DuckDB writes a list of strings as text.
    fn encode_set(&self, _column: &Column, members: &[String]) -> String {
        serde_json::Value::from(members).to_string()
    }

    /// Dates are read as text, which keeps the microseconds a cursor needs to name a
    /// row's position exactly.
    fn select(&self, column: &Column) -> String {
        if column.kind != FieldKind::Date {
            return column.sql.clone();
        }
        match self.date_of(column) {
            DateStorage::Text => column.sql.clone(),
            DateStorage::Date => format!("strftime({}, '%Y-%m-%d')", column.sql),
            _ => format!("strftime({}, {ISO_MICROS})", self.as_utc(column)),
        }
    }
}

/// A DuckDB connection, shared between threads one statement at a time.
pub struct DuckDbRunner {
    connection: Mutex<Connection>,
}

impl DuckDbRunner {
    /// Runs a closure with the connection, for work outside the backend such as loading rows.
    pub fn with<T>(&self, work: impl FnOnce(&Connection) -> T) -> T {
        work(&self.connection.lock().unwrap_or_else(|poisoned| poisoned.into_inner()))
    }
}

fn cell(value: Value) -> Result<Cell, String> {
    Ok(match value {
        Value::Null => Cell::Null,
        Value::Boolean(value) => Cell::Int(i64::from(value)),
        Value::TinyInt(value) => Cell::Int(i64::from(value)),
        Value::SmallInt(value) => Cell::Int(i64::from(value)),
        Value::Int(value) => Cell::Int(i64::from(value)),
        Value::BigInt(value) => Cell::Int(value),
        Value::UTinyInt(value) => Cell::Int(i64::from(value)),
        Value::USmallInt(value) => Cell::Int(i64::from(value)),
        Value::UInt(value) => Cell::Int(i64::from(value)),
        Value::UBigInt(value) => Cell::Int(i64::try_from(value).unwrap_or(i64::MAX)),
        Value::HugeInt(value) => Cell::Int(i64::try_from(value).unwrap_or(i64::MAX)),
        Value::Float(value) => Cell::Real(f64::from(value)),
        Value::Double(value) => Cell::Real(value),
        Value::Decimal(value) => Cell::Text(value.to_string()),
        Value::Text(text) | Value::Enum(text) => Cell::Text(text),
        // A set comes back as a native list, and is handed on as the JSON array the
        // compiler expects.
        Value::List(members) | Value::Array(members) => {
            let mut texts = Vec::with_capacity(members.len());
            for member in members {
                match member {
                    Value::Text(text) | Value::Enum(text) => texts.push(text),
                    other => return Err(format!("a list holds {other:?}, not text")),
                }
            }
            Cell::Text(serde_json::Value::from(texts).to_string())
        }
        other => return Err(format!("cannot read a column holding {other:?}")),
    })
}

impl SqlRunner for DuckDbRunner {
    fn run(&self, sql: &str, params: &[Param]) -> Result<Vec<Vec<Cell>>, String> {
        let bound = params.iter().map(|param| match param {
            Param::Text(text) => Value::Text(text.clone()),
            Param::Int(value) => Value::BigInt(*value),
            Param::Real(value) => Value::Double(*value),
        });
        self.with(|connection| {
            let mut statement = connection.prepare_cached(sql).map_err(|error| error.to_string())?;
            let mut rows = statement.query(params_from_iter(bound)).map_err(|error| error.to_string())?;
            let mut found = Vec::new();
            while let Some(row) = rows.next().map_err(|error| error.to_string())? {
                let width = row.as_ref().column_count();
                let mut cells = Vec::with_capacity(width);
                for index in 0..width {
                    let value: Value = row.get(index).map_err(|error| error.to_string())?;
                    cells.push(cell(value)?);
                }
                found.push(cells);
            }
            Ok(found)
        })
    }
}

pub type DuckDbBackend = SqlBackend<DuckDbDialect, DuckDbRunner>;

/// A backend over a DuckDB connection, querying the table or view named after the
/// collection.
pub fn duckdb_backend(
    collection: &Collection,
    connection: Connection,
    options: DuckDbOptions,
) -> Result<DuckDbBackend, DefinitionError> {
    options.check(collection)?;
    let runner = DuckDbRunner { connection: Mutex::new(connection) };
    Ok(SqlBackend::new(collection, DuckDbDialect::new(options), runner))
}
