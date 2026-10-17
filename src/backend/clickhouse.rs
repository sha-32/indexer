//! ClickHouse, over its HTTP interface.
//!
//! Set fields are `Array(String)` columns. Ordinal and keyword fields may be `String`,
//! `LowCardinality(String)` or `Enum` columns. Text is compared exactly, byte by byte,
//! and regular expressions are matched by ClickHouse's own linear-time engine.

use std::collections::HashMap;

use serde_json::Value as Json;
use ureq::Agent;

use super::sql::{Cell, Column, Dialect, Param, SqlBackend, SqlRunner};
use crate::collection::{Collection, DefinitionError};
use crate::schema::FieldKind;

/// How a date field is stored.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum DateStorage {
    /// A `DateTime64` or `DateTime`. Rows come back as UTC ISO timestamps with six
    /// fractional digits, whatever time zone the column is declared with.
    #[default]
    DateTime,
    /// A `Date` or `Date32`. Literals and `now` are compared by their UTC day, and rows
    /// come back as `YYYY-MM-DD`.
    Date,
    /// ISO-8601 UTC strings, compared as text.
    Text,
}

/// How a collection's fields are stored in ClickHouse.
#[derive(Clone, Debug, Default)]
pub struct ClickHouseOptions {
    dates: DateStorage,
    date_columns: HashMap<String, DateStorage>,
}

impl ClickHouseOptions {
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

pub struct ClickHouseDialect {
    options: ClickHouseOptions,
}

impl ClickHouseDialect {
    pub fn new(options: ClickHouseOptions) -> ClickHouseDialect {
        ClickHouseDialect { options }
    }

    fn date_of(&self, column: &Column) -> DateStorage {
        *self.options.date_columns.get(column.field).unwrap_or(&self.options.dates)
    }

    /// A date column as a `DateTime64(6, 'UTC')`, whatever it is stored as.
    fn as_utc(&self, column: &Column) -> String {
        let col = &column.sql;
        match self.date_of(column) {
            DateStorage::DateTime | DateStorage::Date => format!("toDateTime64({col}, 6, 'UTC')"),
            DateStorage::Text => format!("parseDateTime64BestEffort({col}, 6, 'UTC')"),
        }
    }
}

impl Dialect for ClickHouseDialect {
    fn ident(&self, name: &str) -> String {
        format!("`{}`", name.replace('\\', "\\\\").replace('`', "\\`"))
    }

    /// A placeholder names its parameter and states its type.
    fn placeholder(&self, position: usize, param: &Param) -> String {
        let kind = match param {
            Param::Text(_) => "String",
            Param::Int(_) => "Int64",
            Param::Real(_) => "Float64",
        };
        format!("{{p{position}:{kind}}}")
    }

    fn lower(&self, expression: &str) -> String {
        format!("lowerUTF8({expression})")
    }

    fn contains(&self, column: &Column, placeholder: &str) -> Option<String> {
        Some(format!("positionCaseInsensitiveUTF8({}, {placeholder}) > 0", column.sql))
    }

    fn regex(&self, column: &Column, placeholder: &str) -> String {
        format!("match({}, concat('(?i)', {placeholder}))", column.sql)
    }

    fn date(&self, column: &Column, placeholder: &str, value: &str) -> (String, String) {
        match self.date_of(column) {
            DateStorage::DateTime => {
                (format!("parseDateTime64BestEffort({placeholder}, 6, 'UTC')"), value.to_string())
            }
            DateStorage::Date => (format!("toDate({placeholder})"), value.chars().take(10).collect()),
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
        format!("hasAll({}, JSONExtract({placeholder}, 'Array(String)'))", column.sql)
    }

    fn set_within(&self, column: &Column, placeholder: &str) -> String {
        format!("hasAll(JSONExtract({placeholder}, 'Array(String)'), {})", column.sql)
    }

    fn set_count(&self, column: &Column) -> String {
        format!("length({})", column.sql)
    }

    fn encode_set(&self, _column: &Column, members: &[String]) -> String {
        Json::from(members).to_string()
    }

    /// Everything is read as text or a plain number. For dates that keeps the
    /// microseconds a cursor needs to name a row's position exactly.
    fn select(&self, column: &Column) -> String {
        let col = &column.sql;
        match (column.kind, self.date_of(column)) {
            (FieldKind::Set, _) => format!("toJSONString({col})"),
            (FieldKind::Date, DateStorage::Text) => col.clone(),
            (FieldKind::Date, DateStorage::Date) => format!("toString({col})"),
            (FieldKind::Date, DateStorage::DateTime) => {
                format!("concat(replaceOne(toString({}), ' ', 'T'), 'Z')", self.as_utc(column))
            }
            (FieldKind::Number, _) => col.clone(),
            _ => format!("toString({col})"),
        }
    }
}

/// How to reach a ClickHouse server's HTTP interface.
#[derive(Clone, Debug)]
pub struct ClickHouseClient {
    url: String,
    user: Option<String>,
    password: Option<String>,
    database: Option<String>,
    agent: Agent,
}

impl ClickHouseClient {
    /// A client for the server at a URL such as `http://localhost:8123`.
    pub fn new(url: impl Into<String>) -> ClickHouseClient {
        // An error status carries the server's explanation in its body, which is
        // wanted, so the status is checked here and not by the HTTP client.
        let agent = Agent::config_builder().http_status_as_error(false).build().into();
        ClickHouseClient { url: url.into(), user: None, password: None, database: None, agent }
    }

    pub fn user(mut self, user: impl Into<String>) -> Self {
        self.user = Some(user.into());
        self
    }

    pub fn password(mut self, password: impl Into<String>) -> Self {
        self.password = Some(password.into());
        self
    }

    pub fn database(mut self, database: impl Into<String>) -> Self {
        self.database = Some(database.into());
        self
    }

    /// Sends one statement, with its parameters, and returns the response body.
    fn send(&self, sql: &str, params: &[Param]) -> Result<String, String> {
        let mut request = self.agent.post(&self.url);
        if let Some(user) = &self.user {
            request = request.header("X-ClickHouse-User", user);
        }
        if let Some(password) = &self.password {
            request = request.header("X-ClickHouse-Key", password);
        }
        if let Some(database) = &self.database {
            request = request.query("database", database);
        }
        for (index, param) in params.iter().enumerate() {
            let value = match param {
                Param::Text(text) => escape(text),
                Param::Int(value) => value.to_string(),
                Param::Real(value) => value.to_string(),
            };
            request = request.query(format!("param_p{}", index + 1), value);
        }
        let mut response = request.send(sql).map_err(|error| error.to_string())?;
        let body = response.body_mut().read_to_string().map_err(|error| error.to_string())?;
        if response.status().is_success() { Ok(body) } else { Err(body.trim().to_string()) }
    }

    /// Runs a statement that returns nothing, such as creating a table or loading rows.
    pub fn execute(&self, sql: &str) -> Result<(), String> {
        self.send(sql, &[]).map(|_| ())
    }
}

/// A string parameter travels in ClickHouse's escaped text form, where a backslash
/// starts an escape and a tab or a newline would end the value.
fn escape(text: &str) -> String {
    let mut escaped = String::with_capacity(text.len());
    for ch in text.chars() {
        match ch {
            '\\' => escaped.push_str("\\\\"),
            '\t' => escaped.push_str("\\t"),
            '\n' => escaped.push_str("\\n"),
            '\r' => escaped.push_str("\\r"),
            '\0' => escaped.push_str("\\0"),
            other => escaped.push(other),
        }
    }
    escaped
}

impl SqlRunner for ClickHouseClient {
    fn run(&self, sql: &str, params: &[Param]) -> Result<Vec<Vec<Cell>>, String> {
        let body = self.send(&format!("{sql} FORMAT JSONCompact"), params)?;
        let parsed: Json = serde_json::from_str(&body).map_err(|error| error.to_string())?;
        let Some(rows) = parsed["data"].as_array() else {
            return Err("the response has no data".to_string());
        };
        rows.iter()
            .map(|row| {
                let cells = row.as_array().ok_or("a row is not an array")?;
                cells
                    .iter()
                    .map(|value| {
                        Ok(match value {
                            Json::Null => Cell::Null,
                            Json::Bool(value) => Cell::Int(i64::from(*value)),
                            Json::Number(number) => match number.as_i64() {
                                Some(value) => Cell::Int(value),
                                None => Cell::Real(number.as_f64().ok_or("a number is out of range")?),
                            },
                            // 64-bit integers and decimals arrive quoted. The compiler
                            // reads a number out of text where the field is a number.
                            Json::String(text) => Cell::Text(text.clone()),
                            other => return Err(format!("cannot read a column holding {other}")),
                        })
                    })
                    .collect()
            })
            .collect()
    }
}

pub type ClickHouseBackend = SqlBackend<ClickHouseDialect, ClickHouseClient>;

/// A backend over a ClickHouse server, querying the table named after the collection.
pub fn clickhouse_backend(
    collection: &Collection,
    client: ClickHouseClient,
    options: ClickHouseOptions,
) -> Result<ClickHouseBackend, DefinitionError> {
    options.check(collection)?;
    Ok(SqlBackend::new(collection, ClickHouseDialect::new(options), client))
}
