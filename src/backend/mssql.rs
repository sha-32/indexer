//! Microsoft SQL Server.
//!
//! Set fields are JSON arrays of strings held as text, read with `OPENJSON`.
//!
//! Three things follow the server and not this crate. Text is compared under the
//! column's collation, which by default ignores letter case, so a keyword matches in
//! any case and text sorts without regard to it. Regular expressions exist only from
//! SQL Server 2025 on, so they are refused unless [`MsSqlOptions::regex`] turns them on.
//! And the driver is asynchronous, so the backend runs it on a runtime of its own: call
//! it from a blocking context, never straight from an async task.

use std::collections::HashMap;
use std::error::Error as StdError;
use std::sync::Mutex;

use tiberius::{Client, ColumnData, Config, ToSql};
use tokio::net::TcpStream;
use tokio::runtime::{Builder, Runtime};
use tokio_util::compat::{Compat, TokioAsyncWriteCompatExt};

use super::sql::{Cell, Column, Dialect, Param, SqlBackend, SqlRunner};
use crate::collection::{Collection, DefinitionError};
use crate::schema::FieldKind;

/// How a date field is stored.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum DateStorage {
    /// A `DATETIME2` holding UTC. Rows come back as UTC ISO timestamps with six
    /// fractional digits.
    #[default]
    Datetime2,
    /// A `DATETIMEOFFSET`. Values are compared and returned as UTC instants, whatever
    /// offset each was stored with.
    Datetimeoffset,
    /// Calendar days. Literals and `now` are compared by their UTC day, and rows come
    /// back as `YYYY-MM-DD`.
    Date,
    /// ISO-8601 UTC strings, compared as text.
    Text,
}

/// How a collection's fields are stored in SQL Server.
#[derive(Clone, Debug, Default)]
pub struct MsSqlOptions {
    dates: DateStorage,
    date_columns: HashMap<String, DateStorage>,
    regex: bool,
}

impl MsSqlOptions {
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

    /// Match regular expressions with `REGEXP_LIKE`. This needs SQL Server 2025 and a
    /// database at compatibility level 170 or above.
    pub fn regex(mut self) -> Self {
        self.regex = true;
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

pub struct MsSqlDialect {
    options: MsSqlOptions,
}

/// An ISO instant in the form SQL Server reads without a time zone: `2026-01-02 09:30:00`.
fn naive(iso: &str) -> String {
    iso.trim_end_matches('Z').replacen('T', " ", 1)
}

/// Whether no member of the JSON array `inner` is missing from the JSON array `outer`.
fn subset(inner: &str, outer: &str) -> String {
    format!(
        "NOT EXISTS (SELECT 1 FROM OPENJSON({inner}) AS m WHERE m.[value] NOT IN (SELECT [value] FROM OPENJSON({outer})))"
    )
}

impl MsSqlDialect {
    pub fn new(options: MsSqlOptions) -> MsSqlDialect {
        MsSqlDialect { options }
    }

    fn date_of(&self, column: &Column) -> DateStorage {
        *self.options.date_columns.get(column.field).unwrap_or(&self.options.dates)
    }

    /// A date column as a `DATETIME2(6)` in UTC, whatever it is stored as.
    fn as_utc(&self, column: &Column) -> String {
        let col = &column.sql;
        match self.date_of(column) {
            DateStorage::Datetime2 => format!("CAST({col} AS DATETIME2(6))"),
            DateStorage::Datetimeoffset => format!("CAST(SWITCHOFFSET({col}, '+00:00') AS DATETIME2(6))"),
            DateStorage::Date => format!("CAST({col} AS DATETIME2(6))"),
            DateStorage::Text => format!("CAST(REPLACE(REPLACE({col}, 'T', ' '), 'Z', '') AS DATETIME2(6))"),
        }
    }
}

impl Dialect for MsSqlDialect {
    fn ident(&self, name: &str) -> String {
        format!("[{}]", name.replace(']', "]]"))
    }

    fn placeholder(&self, position: usize, _param: &Param) -> String {
        format!("@P{position}")
    }

    fn limit(&self, limit: &str, offset: &str) -> String {
        format!("OFFSET {offset} ROWS FETCH NEXT {limit} ROWS ONLY")
    }

    fn offset_first(&self) -> bool {
        true
    }

    /// `LIKE` here also reads `[` as the start of a character class.
    fn like_special(&self, ch: char) -> bool {
        matches!(ch, '!' | '%' | '_' | '[')
    }

    fn supports_regex(&self) -> bool {
        self.options.regex
    }

    fn regex(&self, column: &Column, placeholder: &str) -> String {
        format!("REGEXP_LIKE({}, {placeholder}, 'i')", column.sql)
    }

    fn date(&self, column: &Column, placeholder: &str, value: &str) -> (String, String) {
        match self.date_of(column) {
            DateStorage::Datetime2 => (format!("CAST({placeholder} AS DATETIME2(6))"), naive(value)),
            DateStorage::Datetimeoffset => {
                let instant = if value.len() == 10 { format!("{value} 00:00:00") } else { naive(value) };
                (format!("CAST({placeholder} AS DATETIMEOFFSET(6))"), format!("{instant} +00:00"))
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
        subset(placeholder, &column.sql)
    }

    fn set_within(&self, column: &Column, placeholder: &str) -> String {
        subset(&column.sql, placeholder)
    }

    fn set_count(&self, column: &Column) -> String {
        format!("(SELECT COUNT(*) FROM OPENJSON({}))", column.sql)
    }

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
            DateStorage::Date => format!("CONVERT(CHAR(10), {}, 23)", column.sql),
            // Style 126 leaves the fraction off when it is zero, so it is written out apart.
            _ => {
                let utc = self.as_utc(column);
                format!(
                    "CONVERT(VARCHAR(19), {utc}, 126) + '.' + RIGHT('000000' + CAST(DATEPART(MICROSECOND, {utc}) AS VARCHAR(6)), 6) + 'Z'"
                )
            }
        }
    }
}

type Connection = Client<Compat<TcpStream>>;

/// A SQL Server connection with the runtime that drives it, shared between threads one
/// statement at a time.
pub struct MsSqlRunner {
    state: Mutex<(Runtime, Connection)>,
}

impl MsSqlRunner {
    /// Runs statements outside the backend, such as creating tables or loading rows.
    pub fn batch(&self, sql: &str) -> Result<(), String> {
        let mut state = self.state.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let (runtime, client) = &mut *state;
        runtime
            .block_on(async { client.simple_query(sql).await?.into_results().await })
            .map(|_| ())
            .map_err(|error| error.to_string())
    }
}

fn cell(value: ColumnData<'static>) -> Result<Cell, String> {
    Ok(match value {
        ColumnData::U8(value) => value.map_or(Cell::Null, |value| Cell::Int(i64::from(value))),
        ColumnData::I16(value) => value.map_or(Cell::Null, |value| Cell::Int(i64::from(value))),
        ColumnData::I32(value) => value.map_or(Cell::Null, |value| Cell::Int(i64::from(value))),
        ColumnData::I64(value) => value.map_or(Cell::Null, Cell::Int),
        ColumnData::Bit(value) => value.map_or(Cell::Null, |value| Cell::Int(i64::from(value))),
        ColumnData::F32(value) => value.map_or(Cell::Null, |value| Cell::Real(f64::from(value))),
        ColumnData::F64(value) => value.map_or(Cell::Null, Cell::Real),
        ColumnData::Numeric(value) => value.map_or(Cell::Null, |value| Cell::Real(f64::from(value))),
        ColumnData::String(value) => value.map_or(Cell::Null, |text| Cell::Text(text.into_owned())),
        other => return Err(format!("cannot read a column holding {other:?}")),
    })
}

impl SqlRunner for MsSqlRunner {
    fn run(&self, sql: &str, params: &[Param]) -> Result<Vec<Vec<Cell>>, String> {
        let bound: Vec<&dyn ToSql> = params
            .iter()
            .map(|param| match param {
                Param::Text(text) => text as &dyn ToSql,
                Param::Int(value) => value as &dyn ToSql,
                Param::Real(value) => value as &dyn ToSql,
            })
            .collect();

        let mut state = self.state.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let (runtime, client) = &mut *state;
        let rows = runtime
            .block_on(async { client.query(sql, &bound).await?.into_first_result().await })
            .map_err(|error| error.to_string())?;
        rows.into_iter().map(|row| row.into_iter().map(cell).collect()).collect()
    }
}

pub type MsSqlBackend = SqlBackend<MsSqlDialect, MsSqlRunner>;

/// Connects to SQL Server and returns a backend querying the table named after the
/// collection.
///
/// The connection lives on a runtime owned by the backend, so this and every search
/// must be called from a blocking context.
pub fn mssql_backend(
    collection: &Collection,
    config: Config,
    options: MsSqlOptions,
) -> Result<MsSqlBackend, Box<dyn StdError + Send + Sync>> {
    options.check(collection)?;
    let runtime = Builder::new_current_thread().enable_all().build()?;
    let client = runtime.block_on(async {
        let tcp = TcpStream::connect(config.get_addr()).await?;
        tcp.set_nodelay(true)?;
        Client::connect(config, tcp.compat_write()).await
    })?;
    let runner = MsSqlRunner { state: Mutex::new((runtime, client)) };
    Ok(SqlBackend::new(collection, MsSqlDialect::new(options), runner))
}
