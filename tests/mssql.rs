//! These run against real SQL Server instances, and each is skipped unless its variable
//! (MSSQL_URL for 2022, MSSQL2025_URL for 2025) holds a connection string.
//! `.cargo/config.toml` points them at the local containers.

mod common;

use common::suite::{Returned, Store, check_store, insert_sql, literal};
use common::{ids, query, rows, tickets};
use indexer::backend::mssql::{DateStorage, MsSqlOptions, mssql_backend};
use indexer::{Backend, PageRequest};
use serde_json::json;
use tiberius::Config;

/// One way the tickets can be laid out in SQL Server.
struct Layout {
    name: &'static str,
    options: MsSqlOptions,
    ddl: &'static str,
    created: DateStorage,
    closed: DateStorage,
}

/// What is stored for a date of the example rows.
fn stored(iso: &str, storage: DateStorage) -> String {
    let naive = || {
        if iso.len() == 10 { format!("{iso} 00:00:00") } else { iso.trim_end_matches('Z').replace('T', " ") }
    };
    match storage {
        DateStorage::Text => iso.to_string(),
        DateStorage::Date => iso[..10].to_string(),
        DateStorage::Datetime2 => naive(),
        DateStorage::Datetimeoffset => format!("{} +00:00", naive()),
    }
}

fn returned(storage: DateStorage) -> Returned {
    match storage {
        DateStorage::Text => Returned::AsWritten,
        DateStorage::Date => Returned::Day,
        DateStorage::Datetime2 | DateStorage::Datetimeoffset => Returned::Micros,
    }
}

fn layouts() -> Vec<Layout> {
    use DateStorage::{Date, Datetime2, Datetimeoffset, Text};
    vec![
        Layout {
            name: "text dates",
            options: MsSqlOptions::default().dates(Text),
            ddl: "CREATE TABLE tickets (
                id INT PRIMARY KEY, title NVARCHAR(200) NOT NULL, body NVARCHAR(MAX), status NVARCHAR(20) NOT NULL,
                priority NVARCHAR(20) NOT NULL, labels NVARCHAR(MAX) NOT NULL, estimate FLOAT, spent FLOAT,
                created NVARCHAR(40) NOT NULL, closed_at NVARCHAR(40), assignee NVARCHAR(50),
                story NVARCHAR(50) NOT NULL, archived INT NOT NULL
            )",
            created: Text,
            closed: Text,
        },
        Layout {
            name: "datetime2 dates holding UTC",
            options: MsSqlOptions::default(),
            ddl: "CREATE TABLE tickets (
                id BIGINT PRIMARY KEY, title VARCHAR(200) NOT NULL, body VARCHAR(MAX), status VARCHAR(20) NOT NULL,
                priority VARCHAR(20) NOT NULL, labels VARCHAR(MAX) NOT NULL, estimate INT, spent DECIMAL(10, 2),
                created DATETIME2(6) NOT NULL, closed_at DATETIME2(7), assignee VARCHAR(50),
                story VARCHAR(50) NOT NULL, archived SMALLINT NOT NULL
            )",
            created: Datetime2,
            closed: Datetime2,
        },
        Layout {
            name: "datetimeoffset dates",
            options: MsSqlOptions::default().dates(Datetimeoffset),
            ddl: "CREATE TABLE tickets (
                id INT PRIMARY KEY, title NVARCHAR(200) NOT NULL, body NVARCHAR(MAX), status NVARCHAR(20) NOT NULL,
                priority NVARCHAR(20) NOT NULL, labels NVARCHAR(MAX) NOT NULL, estimate INT, spent INT,
                created DATETIMEOFFSET(6) NOT NULL, closed_at DATETIMEOFFSET(6), assignee NVARCHAR(50),
                story NVARCHAR(50) NOT NULL, archived INT NOT NULL
            )",
            created: Datetimeoffset,
            closed: Datetimeoffset,
        },
        Layout {
            name: "date columns",
            options: MsSqlOptions::default().dates(Date),
            ddl: "CREATE TABLE tickets (
                id INT PRIMARY KEY, title NVARCHAR(200) NOT NULL, body NVARCHAR(MAX), status NVARCHAR(20) NOT NULL,
                priority NVARCHAR(20) NOT NULL, labels NVARCHAR(MAX) NOT NULL, estimate INT, spent INT,
                created DATE NOT NULL, closed_at DATE, assignee NVARCHAR(50),
                story NVARCHAR(50) NOT NULL, archived INT NOT NULL
            )",
            created: Date,
            closed: Date,
        },
        Layout {
            // Each field follows its own storage, and `closed>created` compares across the two.
            name: "per-field storage",
            options: MsSqlOptions::default().dates(Date).date_column("created", Datetimeoffset),
            ddl: "CREATE TABLE tickets (
                id INT PRIMARY KEY, title NVARCHAR(200) NOT NULL, body NVARCHAR(MAX), status NVARCHAR(20) NOT NULL,
                priority NVARCHAR(20) NOT NULL, labels NVARCHAR(MAX) NOT NULL, estimate INT, spent INT,
                created DATETIMEOFFSET(6) NOT NULL, closed_at DATE, assignee NVARCHAR(50),
                story NVARCHAR(50) NOT NULL, archived INT NOT NULL
            )",
            created: Datetimeoffset,
            closed: Date,
        },
        Layout {
            name: "text against datetime2",
            options: MsSqlOptions::default().dates(Text).date_column("created", Datetime2),
            ddl: "CREATE TABLE tickets (
                id INT PRIMARY KEY, title NVARCHAR(200) NOT NULL, body NVARCHAR(MAX), status NVARCHAR(20) NOT NULL,
                priority NVARCHAR(20) NOT NULL, labels NVARCHAR(MAX) NOT NULL, estimate INT, spent INT,
                created DATETIME2(6) NOT NULL, closed_at NVARCHAR(40), assignee NVARCHAR(50),
                story NVARCHAR(50) NOT NULL, archived INT NOT NULL
            )",
            created: Datetime2,
            closed: Text,
        },
    ]
}

fn config(variable: &str) -> Option<Config> {
    let connection = std::env::var(variable).unwrap_or_default();
    if connection.is_empty() {
        eprintln!("skipped: {variable} is not set");
        return None;
    }
    Some(Config::from_ado_string(&connection).unwrap_or_else(|error| panic!("{variable}: {error}")))
}

fn check_server(variable: &str, engine: &str, regex: bool) {
    for (index, layout) in layouts().iter().enumerate() {
        let Some(config) = config(variable) else { return };
        let options = if regex { layout.options.clone().regex() } else { layout.options.clone() };
        let backend =
            mssql_backend(&tickets(), config, options).unwrap_or_else(|error| panic!("{engine}: {error}"));
        let execute =
            |sql: &str| backend.runner().batch(sql).unwrap_or_else(|error| panic!("{sql}: {error}"));

        let database = format!("indexer_test_{index}");
        execute(&format!("DROP DATABASE IF EXISTS {database}"));
        execute(&format!("CREATE DATABASE {database}"));
        execute(&format!("USE {database}"));
        execute(layout.ddl);

        let labels = |members: &[&str]| literal(Some(json!(members).to_string()));
        let date = |field: &str, iso: &str| {
            stored(iso, if field == "created" { layout.created } else { layout.closed })
        };
        let insert = |row: &indexer::Row| insert_sql(row, &labels, &date);
        for row in rows() {
            execute(&insert(&row));
        }
        check_store(&Store {
            name: format!("{engine}, {}", layout.name),
            backend: &backend,
            created: returned(layout.created),
            closed: returned(layout.closed),
            regex,
            thorough: true,
            execute: &execute,
            insert: &insert,
        });

        // LIKE reads a bracket as the start of a character class unless it is escaped.
        execute("UPDATE tickets SET title = 'Fix [urgent] flag' WHERE id = 2");
        let tickets = tickets();
        for (input, expected) in [("\"[urgent]\"", vec![2]), ("\"[u\"", vec![2]), ("\"[a-z]\"", vec![])] {
            let page = backend.search(&query(&tickets, input), &PageRequest::first(10)).unwrap();
            assert_eq!(ids(&page.rows), expected, "{engine}, {}: {input}", layout.name);
        }

        execute("USE master");
        execute(&format!("DROP DATABASE {database}"));
    }
}

#[test]
fn sql_server_2022_passes_the_store_checks_in_every_layout() {
    check_server("MSSQL_URL", "sql server 2022", false);
}

#[test]
fn sql_server_2025_passes_the_store_checks_with_regular_expressions() {
    check_server("MSSQL2025_URL", "sql server 2025", true);
}

#[test]
fn a_storage_must_fit_the_field_it_is_given_to() {
    let Some(config) = config("MSSQL_URL") else { return };
    let options = MsSqlOptions::default().date_column("title", DateStorage::Date);
    assert!(mssql_backend(&tickets(), config, options).is_err());
}
