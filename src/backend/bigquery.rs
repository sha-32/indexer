//! Google BigQuery, over its REST API.
//!
//! Set fields are `ARRAY<STRING>` columns. Text is compared exactly, and regular
//! expressions are matched by BigQuery's own linear-time engine.
//!
//! BigQuery bills by the data a query scans, and each search runs two queries: the page
//! and the count. Partition and cluster the table on the fields people filter by.

use std::collections::HashMap;
use std::fmt;
use std::sync::Arc;
use std::thread::sleep;
use std::time::{Duration, Instant};

use serde_json::{Value as Json, json};
use ureq::Agent;

use super::sql::{Cell, Column, Dialect, Param, SqlBackend, SqlRunner};
use crate::collection::{Collection, DefinitionError};
use crate::schema::FieldKind;

/// How a date field is stored.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum DateStorage {
    /// A `TIMESTAMP`. Rows come back as UTC ISO timestamps with six fractional digits.
    #[default]
    Timestamp,
    /// A `DATETIME` holding UTC. Rows come back as for `Timestamp`.
    Datetime,
    /// Calendar days. Literals and `now` are compared by their UTC day, and rows come
    /// back as `YYYY-MM-DD`.
    Date,
    /// ISO-8601 UTC strings, compared as text.
    Text,
}

/// How a collection's fields are stored in BigQuery.
#[derive(Clone, Debug, Default)]
pub struct BigQueryOptions {
    dates: DateStorage,
    date_columns: HashMap<String, DateStorage>,
}

impl BigQueryOptions {
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

pub struct BigQueryDialect {
    options: BigQueryOptions,
}

/// An ISO instant without its `T` and `Z`: `2026-01-02 09:30:00`.
fn naive(iso: &str) -> String {
    iso.trim_end_matches('Z').replacen('T', " ", 1)
}

const ISO_MICROS: &str = "'%Y-%m-%dT%H:%M:%E6SZ'";

/// Whether no member of the array `inner` is missing from the array `outer`.
fn subset(inner: &str, outer: &str) -> String {
    format!("NOT EXISTS (SELECT 1 FROM UNNEST({inner}) AS m WHERE m NOT IN UNNEST({outer}))")
}

impl BigQueryDialect {
    pub fn new(options: BigQueryOptions) -> BigQueryDialect {
        BigQueryDialect { options }
    }

    fn date_of(&self, column: &Column) -> DateStorage {
        *self.options.date_columns.get(column.field).unwrap_or(&self.options.dates)
    }

    /// A date column as a `TIMESTAMP`, whatever it is stored as.
    fn as_instant(&self, column: &Column) -> String {
        match self.date_of(column) {
            DateStorage::Timestamp => column.sql.clone(),
            _ => format!("TIMESTAMP({}, 'UTC')", column.sql),
        }
    }
}

impl Dialect for BigQueryDialect {
    fn ident(&self, name: &str) -> String {
        format!("`{}`", name.replace('\\', "\\\\").replace('`', "\\`"))
    }

    /// Parameters are named after their position, so each is bound by name wherever
    /// its placeholder falls in the statement.
    fn placeholder(&self, position: usize, _param: &Param) -> String {
        format!("@p{position}")
    }

    /// `LIKE` here has no `ESCAPE`, so a contains test is a search for the position.
    fn contains(&self, column: &Column, placeholder: &str) -> Option<String> {
        Some(format!("STRPOS(LOWER({}), {placeholder}) > 0", column.sql))
    }

    fn regex(&self, column: &Column, placeholder: &str) -> String {
        format!("REGEXP_CONTAINS({}, CONCAT('(?i)', {placeholder}))", column.sql)
    }

    fn date(&self, column: &Column, placeholder: &str, value: &str) -> (String, String) {
        match self.date_of(column) {
            DateStorage::Timestamp => {
                let instant =
                    if value.len() == 10 { format!("{value}T00:00:00Z") } else { value.to_string() };
                (format!("CAST({placeholder} AS TIMESTAMP)"), instant)
            }
            DateStorage::Datetime => (format!("CAST({placeholder} AS DATETIME)"), naive(value)),
            DateStorage::Date => (format!("CAST({placeholder} AS DATE)"), value.chars().take(10).collect()),
            DateStorage::Text => (placeholder.to_string(), value.to_string()),
        }
    }

    /// Two date fields stored differently are both read as instants.
    fn operand(&self, column: &Column, other: &Column) -> String {
        if column.kind != FieldKind::Date || self.date_of(column) == self.date_of(other) {
            return column.sql.clone();
        }
        self.as_instant(column)
    }

    fn set_has_all(&self, column: &Column, placeholder: &str) -> String {
        subset(&format!("SPLIT({placeholder}, ',')"), &column.sql)
    }

    fn set_within(&self, column: &Column, placeholder: &str) -> String {
        subset(&column.sql, &format!("SPLIT({placeholder}, ',')"))
    }

    fn set_count(&self, column: &Column) -> String {
        format!("ARRAY_LENGTH({})", column.sql)
    }

    /// Members cannot contain a comma, so a comma-separated list splits back exactly.
    fn encode_set(&self, _column: &Column, members: &[String]) -> String {
        members.join(",")
    }

    /// Dates are read as text, which keeps the microseconds a cursor needs to name a
    /// row's position exactly.
    fn select(&self, column: &Column) -> String {
        let col = &column.sql;
        match (column.kind, self.date_of(column)) {
            (FieldKind::Set, _) => format!("TO_JSON_STRING({col})"),
            (FieldKind::Date, DateStorage::Timestamp) => {
                format!("FORMAT_TIMESTAMP({ISO_MICROS}, {col}, 'UTC')")
            }
            (FieldKind::Date, DateStorage::Datetime) => format!("FORMAT_DATETIME({ISO_MICROS}, {col})"),
            (FieldKind::Date, DateStorage::Date) => format!("FORMAT_DATE('%Y-%m-%d', {col})"),
            _ => col.clone(),
        }
    }
}

type TokenSource = Arc<dyn Fn() -> Result<String, String> + Send + Sync>;

/// How to reach a BigQuery project.
#[derive(Clone)]
pub struct BigQueryClient {
    endpoint: String,
    project: String,
    dataset: Option<String>,
    location: Option<String>,
    token: Option<TokenSource>,
    agent: Agent,
}

impl fmt::Debug for BigQueryClient {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("BigQueryClient")
            .field("endpoint", &self.endpoint)
            .field("project", &self.project)
            .field("dataset", &self.dataset)
            .finish_non_exhaustive()
    }
}

/// How long to wait for a query before giving up.
const PATIENCE: Duration = Duration::from_secs(300);

impl BigQueryClient {
    /// A client that runs queries in, and bills them to, a project.
    pub fn new(project: impl Into<String>) -> BigQueryClient {
        // An error status carries the service's explanation in its body, which is
        // wanted, so the status is checked here and not by the HTTP client.
        let agent = Agent::config_builder().http_status_as_error(false).build().into();
        BigQueryClient {
            endpoint: "https://bigquery.googleapis.com".to_string(),
            project: project.into(),
            dataset: None,
            location: None,
            token: None,
            agent,
        }
    }

    /// The API's base URL, for an emulator or a private endpoint.
    pub fn endpoint(mut self, url: impl Into<String>) -> Self {
        self.endpoint = url.into().trim_end_matches('/').to_string();
        self
    }

    /// The dataset that unqualified table names belong to.
    pub fn dataset(mut self, dataset: impl Into<String>) -> Self {
        self.dataset = Some(dataset.into());
        self
    }

    /// Where the dataset lives, such as `EU`. Needed when it is not in the US.
    pub fn location(mut self, location: impl Into<String>) -> Self {
        self.location = Some(location.into());
        self
    }

    /// Where to get an OAuth access token. It is asked for one before every request,
    /// so it should cache the token and renew it as it expires.
    pub fn token(mut self, source: impl Fn() -> Result<String, String> + Send + Sync + 'static) -> Self {
        self.token = Some(Arc::new(source));
        self
    }

    fn call(&self, post: Option<&Json>, path: &str, query: &[(&str, &str)]) -> Result<Json, String> {
        let url = format!("{}/bigquery/v2/projects/{}/{path}", self.endpoint, self.project);
        let token = self.token.as_ref().map(|source| source()).transpose()?;
        let bearer = token.map(|token| format!("Bearer {token}"));
        let sent = match post {
            Some(body) => {
                let mut request = self.agent.post(&url).header("Content-Type", "application/json");
                if let Some(bearer) = &bearer {
                    request = request.header("Authorization", bearer);
                }
                for (key, value) in query {
                    request = request.query(key, value);
                }
                request.send(body.to_string())
            }
            None => {
                let mut request = self.agent.get(&url);
                if let Some(bearer) = &bearer {
                    request = request.header("Authorization", bearer);
                }
                for (key, value) in query {
                    request = request.query(key, value);
                }
                request.call()
            }
        };
        let mut response = sent.map_err(|error| error.to_string())?;
        let text = response.body_mut().read_to_string().map_err(|error| error.to_string())?;
        let parsed: Json = serde_json::from_str(&text).unwrap_or(Json::Null);
        if response.status().is_success() {
            return Ok(parsed);
        }
        Err(parsed["error"]["message"].as_str().map_or(text.trim().to_string(), str::to_string))
    }

    /// Runs one query to completion and returns every row, each a list of cells.
    fn query(&self, sql: &str, params: &[Param]) -> Result<Vec<Json>, String> {
        let parameters: Vec<Json> = params
            .iter()
            .enumerate()
            .map(|(index, param)| {
                let (kind, value) = match param {
                    Param::Text(text) => ("STRING", text.clone()),
                    Param::Int(value) => ("INT64", value.to_string()),
                    Param::Real(value) => ("FLOAT64", value.to_string()),
                };
                json!({
                    "name": format!("p{}", index + 1),
                    "parameterType": { "type": kind },
                    "parameterValue": { "value": value },
                })
            })
            .collect();
        let mut body = json!({
            "query": sql,
            "useLegacySql": false,
            "parameterMode": "NAMED",
            "queryParameters": parameters,
            "timeoutMs": 30_000,
        });
        if let Some(dataset) = &self.dataset {
            body["defaultDataset"] = json!({ "projectId": self.project, "datasetId": dataset });
        }
        if let Some(location) = &self.location {
            body["location"] = json!(location);
        }

        let started = Instant::now();
        let mut answer = self.call(Some(&body), "queries", &[])?;
        let mut rows = Vec::new();
        loop {
            // A slow query comes back unfinished, and a long result comes in pages.
            // Either way the rest is asked for by the job's id.
            if answer["jobComplete"] == false {
                if started.elapsed() > PATIENCE {
                    return Err("the query did not finish in time".to_string());
                }
                sleep(Duration::from_millis(500));
            } else if let Some(page) = answer["rows"].as_array() {
                rows.extend(page.iter().cloned());
            }
            let complete = answer["jobComplete"] != false;
            let page_token = answer["pageToken"].as_str().map(str::to_string);
            if complete && page_token.is_none() {
                return Ok(rows);
            }
            let job = answer["jobReference"]["jobId"].as_str().ok_or("the query has no job id")?.to_string();
            let location = answer["jobReference"]["location"].as_str().map(str::to_string);
            let mut query: Vec<(&str, &str)> = Vec::new();
            if let Some(location) = &location {
                query.push(("location", location));
            }
            if let Some(token) = &page_token {
                query.push(("pageToken", token));
            }
            answer = self.call(None, &format!("queries/{job}"), &query)?;
        }
    }

    /// Runs a statement that returns nothing, such as creating a table or loading rows.
    pub fn execute(&self, sql: &str) -> Result<(), String> {
        self.query(sql, &[]).map(|_| ())
    }

    /// Creates a dataset in the project.
    pub fn create_dataset(&self, dataset: &str) -> Result<(), String> {
        let body = json!({ "datasetReference": { "projectId": self.project, "datasetId": dataset } });
        self.call(Some(&body), "datasets", &[]).map(|_| ())
    }
}

impl SqlRunner for BigQueryClient {
    fn run(&self, sql: &str, params: &[Param]) -> Result<Vec<Vec<Cell>>, String> {
        self.query(sql, params)?
            .iter()
            .map(|row| {
                let cells = row["f"].as_array().ok_or("a row has no fields")?;
                cells
                    .iter()
                    .map(|cell| {
                        // Every scalar arrives as a string. The compiler reads a number
                        // out of text where the field is a number.
                        Ok(match &cell["v"] {
                            Json::Null => Cell::Null,
                            Json::String(text) => Cell::Text(text.clone()),
                            other => return Err(format!("cannot read a column holding {other}")),
                        })
                    })
                    .collect()
            })
            .collect()
    }
}

pub type BigQueryBackend = SqlBackend<BigQueryDialect, BigQueryClient>;

/// A backend over a BigQuery dataset, querying the table named after the collection.
pub fn bigquery_backend(
    collection: &Collection,
    client: BigQueryClient,
    options: BigQueryOptions,
) -> Result<BigQueryBackend, DefinitionError> {
    options.check(collection)?;
    Ok(SqlBackend::new(collection, BigQueryDialect::new(options), client))
}
