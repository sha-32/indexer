//! PostgreSQL.
//!
//! Ordinal and keyword fields may be `text` or enum columns. Regular expressions are
//! checked against this crate's syntax when a query is parsed but matched by
//! PostgreSQL's engine, which reads a few constructs differently.

use std::collections::HashMap;
use std::error::Error as StdError;
use std::sync::Mutex;

use postgres::Client;
use postgres::types::private::BytesMut;
use postgres::types::{Format, IsNull, ToSql, Type, to_sql_checked};

use super::sql::{Cell, Column, Dialect, Param, SqlBackend, SqlRunner};
use crate::collection::{Collection, DefinitionError};
use crate::schema::FieldKind;

/// How a date field is stored.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum DateStorage {
    /// A date literal means UTC midnight, whatever the session time zone, and rows come
    /// back as UTC ISO timestamps with all six fractional digits.
    #[default]
    Timestamptz,
    /// Calendar days. Literals and `now` are compared by their UTC day, and rows come
    /// back as `YYYY-MM-DD`.
    Date,
    /// ISO-8601 UTC strings, compared as text.
    Text,
}

/// How a set field is stored. Both are matched with `@>` and `<@`, which a GIN index
/// can serve.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum SetStorage {
    /// A `jsonb` array of strings.
    #[default]
    Jsonb,
    /// A native `text[]`.
    Array,
}

/// How a collection's fields are stored in PostgreSQL.
#[derive(Clone, Debug, Default)]
pub struct PostgresOptions {
    dates: DateStorage,
    sets: SetStorage,
    date_columns: HashMap<String, DateStorage>,
    set_columns: HashMap<String, SetStorage>,
}

impl PostgresOptions {
    /// The storage of date fields that are not given their own.
    pub fn dates(mut self, storage: DateStorage) -> Self {
        self.dates = storage;
        self
    }

    /// The storage of set fields that are not given their own.
    pub fn sets(mut self, storage: SetStorage) -> Self {
        self.sets = storage;
        self
    }

    /// The storage of one date field, by field name.
    pub fn date_column(mut self, field: impl Into<String>, storage: DateStorage) -> Self {
        self.date_columns.insert(field.into(), storage);
        self
    }

    /// The storage of one set field, by field name.
    pub fn set_column(mut self, field: impl Into<String>, storage: SetStorage) -> Self {
        self.set_columns.insert(field.into(), storage);
        self
    }

    /// Checks that every field given its own storage exists and is of the right kind.
    fn check(&self, collection: &Collection) -> Result<(), DefinitionError> {
        let named = self
            .date_columns
            .keys()
            .map(|field| (field, FieldKind::Date))
            .chain(self.set_columns.keys().map(|field| (field, FieldKind::Set)));
        for (field, kind) in named {
            if !collection.fields().any(|(name, declared)| name == field && declared.kind == kind) {
                return Err(DefinitionError(format!(
                    "Collection \"{}\": \"{field}\" is not a {} field",
                    collection.name(),
                    kind.name()
                )));
            }
        }
        Ok(())
    }
}

pub struct PostgresDialect {
    options: PostgresOptions,
}

impl PostgresDialect {
    pub fn new(options: PostgresOptions) -> PostgresDialect {
        PostgresDialect { options }
    }

    fn date_of(&self, column: &Column) -> DateStorage {
        *self.options.date_columns.get(column.field).unwrap_or(&self.options.dates)
    }

    fn is_array(&self, column: &Column) -> bool {
        *self.options.set_columns.get(column.field).unwrap_or(&self.options.sets) == SetStorage::Array
    }

    fn set_cast(&self, column: &Column) -> &'static str {
        if self.is_array(column) { "text[]" } else { "jsonb" }
    }
}

impl Dialect for PostgresDialect {
    fn ident(&self, name: &str) -> String {
        format!("\"{}\"", name.replace('"', "\"\""))
    }

    fn placeholder(&self, position: usize, _param: &Param) -> String {
        format!("${position}")
    }

    fn regex(&self, column: &Column, placeholder: &str) -> String {
        format!("{} ~* {placeholder}", column.sql)
    }

    /// An untyped parameter takes the column's type, and "2.5" is not a valid integer.
    fn number(&self, _column: &Column, placeholder: &str, value: f64) -> String {
        if value.fract() == 0.0 { placeholder.to_string() } else { format!("{placeholder}::float8") }
    }

    fn date(&self, column: &Column, placeholder: &str, value: &str) -> (String, String) {
        match self.date_of(column) {
            DateStorage::Timestamptz => {
                let instant =
                    if value.len() == 10 { format!("{value}T00:00:00Z") } else { value.to_string() };
                (format!("{placeholder}::timestamptz"), instant)
            }
            DateStorage::Date => (format!("{placeholder}::date"), value.chars().take(10).collect()),
            DateStorage::Text => (placeholder.to_string(), value.to_string()),
        }
    }

    /// Two date fields stored differently are compared as instants. PostgreSQL would
    /// otherwise read the day-only side as midnight in the session time zone.
    fn operand(&self, column: &Column, other: &Column) -> String {
        let mine = self.date_of(column);
        if column.kind != FieldKind::Date || mine == self.date_of(other) || mine == DateStorage::Timestamptz {
            return column.sql.clone();
        }
        format!("({}::timestamp AT TIME ZONE 'UTC')", column.sql)
    }

    fn set_has_all(&self, column: &Column, placeholder: &str) -> String {
        format!("{} @> {placeholder}::{}", column.sql, self.set_cast(column))
    }

    fn set_within(&self, column: &Column, placeholder: &str) -> String {
        format!("{} <@ {placeholder}::{}", column.sql, self.set_cast(column))
    }

    fn set_count(&self, column: &Column) -> String {
        if self.is_array(column) {
            format!("cardinality({})", column.sql)
        } else {
            format!("jsonb_array_length({})", column.sql)
        }
    }

    fn encode_set(&self, column: &Column, members: &[String]) -> String {
        if !self.is_array(column) {
            return serde_json::Value::from(members).to_string();
        }
        let quoted: Vec<String> = members
            .iter()
            .map(|member| format!("\"{}\"", member.replace('\\', "\\\\").replace('"', "\\\"")))
            .collect();
        format!("{{{}}}", quoted.join(","))
    }

    /// Everything is read as text, so no column type needs a decoder of its own. For
    /// dates that also keeps the microseconds a cursor needs to name a row's position
    /// exactly, and avoids any conversion through the session time zone.
    fn select(&self, column: &Column) -> String {
        let col = &column.sql;
        match (column.kind, self.date_of(column)) {
            (FieldKind::Set, _) => format!("to_jsonb({col})::text"),
            (FieldKind::Date, DateStorage::Date) => format!("to_char({col}, 'YYYY-MM-DD')"),
            (FieldKind::Date, DateStorage::Timestamptz) => {
                format!("to_char({col} AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS.US\"Z\"')")
            }
            _ => format!("{col}::text"),
        }
    }
}

/// A parameter sent in PostgreSQL's text format, so the server parses it as whatever
/// type the statement calls for: an integer, an enum, a `timestamptz`.
#[derive(Debug)]
struct TextParam(String);

impl ToSql for TextParam {
    fn to_sql(&self, _ty: &Type, out: &mut BytesMut) -> Result<IsNull, Box<dyn StdError + Sync + Send>> {
        out.extend_from_slice(self.0.as_bytes());
        Ok(IsNull::No)
    }

    fn accepts(_ty: &Type) -> bool {
        true
    }

    fn encode_format(&self, _ty: &Type) -> Format {
        Format::Text
    }

    to_sql_checked!();
}

/// A PostgreSQL connection, shared between threads one statement at a time.
pub struct PostgresRunner {
    client: Mutex<Client>,
}

impl PostgresRunner {
    /// Runs a closure with the client, for work outside the backend such as loading rows.
    pub fn with<T>(&self, work: impl FnOnce(&mut Client) -> T) -> T {
        work(&mut self.client.lock().unwrap_or_else(|poisoned| poisoned.into_inner()))
    }
}

impl SqlRunner for PostgresRunner {
    fn run(&self, sql: &str, params: &[Param]) -> Result<Vec<Vec<Cell>>, String> {
        let texts: Vec<TextParam> = params
            .iter()
            .map(|param| {
                TextParam(match param {
                    Param::Text(text) => text.clone(),
                    Param::Int(value) => value.to_string(),
                    Param::Real(value) => value.to_string(),
                })
            })
            .collect();
        let bound: Vec<&(dyn ToSql + Sync)> = texts.iter().map(|text| text as &(dyn ToSql + Sync)).collect();

        self.with(|client| {
            let mut found = Vec::new();
            for row in client.query(sql, &bound)? {
                let mut cells = Vec::with_capacity(row.len());
                for (index, column) in row.columns().iter().enumerate() {
                    cells.push(if *column.type_() == Type::INT8 {
                        row.try_get::<_, Option<i64>>(index)?.map_or(Cell::Null, Cell::Int)
                    } else {
                        row.try_get::<_, Option<String>>(index)?.map_or(Cell::Null, Cell::Text)
                    });
                }
                found.push(cells);
            }
            Ok(found)
        })
        .map_err(|error: postgres::Error| error.to_string())
    }
}

pub type PostgresBackend = SqlBackend<PostgresDialect, PostgresRunner>;

/// A backend over a PostgreSQL connection, querying the table named after the collection.
pub fn postgres_backend(
    collection: &Collection,
    client: Client,
    options: PostgresOptions,
) -> Result<PostgresBackend, DefinitionError> {
    options.check(collection)?;
    let runner = PostgresRunner { client: Mutex::new(client) };
    Ok(SqlBackend::new(collection, PostgresDialect::new(options), runner))
}
