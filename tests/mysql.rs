//! These run against real MySQL and MariaDB servers, and each is skipped unless its
//! variable (MYSQL_URL, MARIADB_URL) names one. `.cargo/config.toml` points them at
//! the local containers.

mod common;

use common::suite::{Returned, Store, check_store, insert_sql, literal};
use common::{rows, tickets};
use indexer::backend::mysql::{DateStorage, MySqlOptions, mysql_backend};
use mysql::prelude::Queryable;
use mysql::{Conn, Opts};
use serde_json::json;

/// One way the tickets can be laid out in MySQL or MariaDB.
struct Layout {
    name: &'static str,
    options: MySqlOptions,
    ddl: &'static str,
    created: DateStorage,
    closed: DateStorage,
}

/// What is stored for a date of the example rows. A datetime is written in the
/// server's own form, in UTC.
fn stored(iso: &str, storage: DateStorage) -> String {
    match storage {
        DateStorage::Text => iso.to_string(),
        DateStorage::Date => iso[..10].to_string(),
        DateStorage::Datetime if iso.len() == 10 => format!("{iso} 00:00:00"),
        DateStorage::Datetime => iso.trim_end_matches('Z').replace('T', " "),
    }
}

fn returned(storage: DateStorage) -> Returned {
    match storage {
        DateStorage::Text => Returned::AsWritten,
        DateStorage::Date => Returned::Day,
        DateStorage::Datetime => Returned::Micros,
    }
}

fn layouts() -> Vec<Layout> {
    use DateStorage::{Date, Datetime, Text};
    vec![
        Layout {
            name: "text dates",
            options: MySqlOptions::default().dates(Text),
            ddl: "CREATE TABLE tickets (
                id INT PRIMARY KEY, title VARCHAR(200) NOT NULL, body TEXT, status VARCHAR(20) NOT NULL,
                priority VARCHAR(20) NOT NULL, labels JSON NOT NULL, estimate DOUBLE, spent DOUBLE,
                created VARCHAR(40) NOT NULL, closed_at VARCHAR(40), assignee VARCHAR(50),
                story VARCHAR(50) NOT NULL, archived INT NOT NULL
            )",
            created: Text,
            closed: Text,
        },
        Layout {
            // The enums are declared out of rank order, so sorting cannot lean on them.
            name: "timestamp and datetime dates and native enums",
            options: MySqlOptions::default(),
            ddl: "CREATE TABLE tickets (
                id BIGINT PRIMARY KEY, title VARCHAR(200) NOT NULL, body TEXT,
                status ENUM('closed', 'open') NOT NULL,
                priority ENUM('urgent', 'high', 'normal', 'low') NOT NULL, labels JSON NOT NULL,
                estimate INT, spent DECIMAL(10, 2), created TIMESTAMP(6) NOT NULL, closed_at DATETIME(6),
                assignee VARCHAR(50), story VARCHAR(50) NOT NULL, archived SMALLINT NOT NULL
            )",
            created: Datetime,
            closed: Datetime,
        },
        Layout {
            name: "date columns",
            options: MySqlOptions::default().dates(Date),
            ddl: "CREATE TABLE tickets (
                id INT PRIMARY KEY, title VARCHAR(200) NOT NULL, body TEXT, status VARCHAR(20) NOT NULL,
                priority VARCHAR(20) NOT NULL, labels JSON NOT NULL, estimate INT, spent INT,
                created DATE NOT NULL, closed_at DATE, assignee VARCHAR(50),
                story VARCHAR(50) NOT NULL, archived INT NOT NULL
            )",
            created: Date,
            closed: Date,
        },
        Layout {
            // Each field follows its own storage, and `closed>created` compares across the two.
            name: "per-field storage",
            options: MySqlOptions::default().dates(Date).date_column("created", Datetime),
            ddl: "CREATE TABLE tickets (
                id INT PRIMARY KEY, title VARCHAR(200) NOT NULL, body TEXT, status VARCHAR(20) NOT NULL,
                priority VARCHAR(20) NOT NULL, labels JSON NOT NULL, estimate INT, spent INT,
                created DATETIME(6) NOT NULL, closed_at DATE, assignee VARCHAR(50),
                story VARCHAR(50) NOT NULL, archived INT NOT NULL
            )",
            created: Datetime,
            closed: Date,
        },
        Layout {
            // ISO text against a real datetime has to be read as one to be compared.
            name: "text against datetime",
            options: MySqlOptions::default().dates(Text).date_column("created", Datetime),
            ddl: "CREATE TABLE tickets (
                id INT PRIMARY KEY, title VARCHAR(200) NOT NULL, body TEXT, status VARCHAR(20) NOT NULL,
                priority VARCHAR(20) NOT NULL, labels JSON NOT NULL, estimate INT, spent INT,
                created DATETIME(6) NOT NULL, closed_at VARCHAR(40), assignee VARCHAR(50),
                story VARCHAR(50) NOT NULL, archived INT NOT NULL
            )",
            created: Datetime,
            closed: Text,
        },
    ]
}

fn server(variable: &str) -> Option<Conn> {
    let url = std::env::var(variable).unwrap_or_default();
    if url.is_empty() {
        eprintln!("skipped: {variable} is not set");
        return None;
    }
    let options = Opts::from_url(&url).unwrap_or_else(|error| panic!("{variable}: {error}"));
    Some(Conn::new(options).unwrap_or_else(|error| panic!("a connection to {variable}: {error}")))
}

fn check_server(variable: &str, engine: &str) {
    // Each layout has a database of its own, so they can all be checked at once.
    let layouts = layouts();
    std::thread::scope(|scope| {
        for (index, layout) in layouts.iter().enumerate() {
            scope.spawn(move || {
                let Some(mut connection) = server(variable) else { return };
                let database = format!("indexer_test_{index}");
                for statement in [
                    format!("DROP DATABASE IF EXISTS {database}"),
                    format!("CREATE DATABASE {database}"),
                    format!("USE {database}"),
                    layout.ddl.to_string(),
                    // A session zone the backend must not leave in place.
                    "SET time_zone = '-08:00'".to_string(),
                ] {
                    connection.query_drop(&statement).unwrap_or_else(|error| panic!("{statement}: {error}"));
                }

                let backend = mysql_backend(&tickets(), connection, layout.options.clone()).unwrap();
                let execute = |sql: &str| {
                    backend.runner().with(|connection| {
                        connection.query_drop(sql).unwrap_or_else(|e| panic!("{sql}: {e}"))
                    })
                };
                let zone: Option<String> = backend
                    .runner()
                    .with(|connection| connection.query_first("SELECT @@session.time_zone").unwrap());
                assert_eq!(zone.as_deref(), Some("+00:00"), "{engine}");

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
                    regex: true,
                    thorough: true,
                    execute: &execute,
                    insert: &insert,
                });
                execute(&format!("DROP DATABASE {database}"));
            });
        }
    });
}

#[test]
fn mysql_passes_the_store_checks_in_every_layout() {
    check_server("MYSQL_URL", "mysql");
}

#[test]
fn mariadb_passes_the_store_checks_in_every_layout() {
    check_server("MARIADB_URL", "mariadb");
}

#[test]
fn a_storage_must_fit_the_field_it_is_given_to() {
    let Some(connection) = server("MYSQL_URL") else { return };
    let options = MySqlOptions::default().date_column("title", DateStorage::Date);
    assert!(mysql_backend(&tickets(), connection, options).is_err());
}
