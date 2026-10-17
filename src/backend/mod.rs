//! Backends: the stores a query can run against.

use std::fmt;

use serde_json::Value as Json;

use crate::ast::{Query, QueryError};

#[cfg(feature = "bigquery")]
pub mod bigquery;
#[cfg(feature = "clickhouse")]
pub mod clickhouse;
#[cfg(feature = "duckdb")]
pub mod duckdb;
#[cfg(feature = "elastic")]
pub mod elastic;
pub mod memory;
#[cfg(feature = "mongodb")]
pub mod mongodb;
#[cfg(feature = "mssql")]
pub mod mssql;
#[cfg(feature = "mysql")]
pub mod mysql;
#[cfg(feature = "postgres")]
pub mod postgres;
pub mod sql;
#[cfg(feature = "sqlite")]
pub mod sqlite;

/// One row: a JSON value per field. A text, keyword, ordinal or date field holds a
/// string, a number field a number, a set field an array of strings, and a nullable
/// field may hold null.
pub type Row = serde_json::Map<String, Json>;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PageRequest {
    pub limit: usize,
    /// The `next` cursor of an earlier page of the same query: return the rows that
    /// follow it. This is how to walk a result. It stays fast at any depth and is stable
    /// when rows are added or removed between requests.
    pub after: Option<String>,
    /// Rows to skip, for jumping to an arbitrary position. Applied after `after`.
    pub offset: usize,
}

impl PageRequest {
    /// The first `limit` rows.
    pub fn first(limit: usize) -> PageRequest {
        PageRequest { limit, after: None, offset: 0 }
    }

    /// The `limit` rows after a cursor.
    pub fn after(limit: usize, cursor: impl Into<String>) -> PageRequest {
        PageRequest { limit, after: Some(cursor.into()), offset: 0 }
    }

    pub fn offset(mut self, offset: usize) -> PageRequest {
        self.offset = offset;
        self
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Page {
    pub rows: Vec<Row>,
    /// Rows matching the query across all pages, after any rollup.
    pub total: u64,
    pub has_more: bool,
    /// The cursor for the page after this one, or `None` if this is the last.
    pub next: Option<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Error {
    /// The query or cursor does not fit the collection. The caller's mistake.
    Query(QueryError),
    /// The store failed. Not for showing to callers.
    Backend(String),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Query(error) => error.fmt(f),
            Error::Backend(message) => f.write_str(message),
        }
    }
}

impl std::error::Error for Error {}

impl From<QueryError> for Error {
    fn from(error: QueryError) -> Error {
        Error::Query(error)
    }
}

/// Executes queries against one store of a collection's rows.
pub trait Backend: Send + Sync {
    fn search(&self, query: &Query, page: &PageRequest) -> Result<Page, Error>;
}
