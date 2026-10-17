//! These run against a real server and are skipped unless DATABASE_URL names one.
//! `.cargo/config.toml` points it at the local container.

mod common;

use common::suite::{Returned, Store, check_store, insert_sql, literal};
use common::{rows, tickets};
use indexer::backend::postgres::{DateStorage, PostgresOptions, SetStorage, postgres_backend};
use postgres::{Client, NoTls};
use serde_json::json;

/// One way the tickets can be laid out in PostgreSQL.
struct Layout {
    name: &'static str,
    options: PostgresOptions,
    ddl: &'static str,
    /// Whether `labels` is a `text[]` and not `jsonb`.
    array_labels: bool,
    created: DateStorage,
    closed: DateStorage,
}

/// What is stored for a date of the example rows.
fn stored(iso: &str, storage: DateStorage) -> String {
    match storage {
        DateStorage::Text => iso.to_string(),
        DateStorage::Date => iso[..10].to_string(),
        DateStorage::Timestamptz if iso.len() == 10 => format!("{iso}T00:00:00Z"),
        DateStorage::Timestamptz => iso.to_string(),
    }
}

fn returned(storage: DateStorage) -> Returned {
    match storage {
        DateStorage::Text => Returned::AsWritten,
        DateStorage::Date => Returned::Day,
        DateStorage::Timestamptz => Returned::Micros,
    }
}

fn layouts() -> Vec<Layout> {
    use DateStorage::{Date, Text, Timestamptz};
    vec![
        Layout {
            name: "jsonb sets and text dates",
            options: PostgresOptions::default().dates(Text),
            ddl: "CREATE TABLE tickets (
                id integer PRIMARY KEY, title text NOT NULL, body text, status text NOT NULL,
                priority text NOT NULL, labels jsonb NOT NULL, estimate float8, spent float8,
                created text NOT NULL, closed_at text, assignee text, story text NOT NULL,
                archived integer NOT NULL
            )",
            array_labels: false,
            created: Text,
            closed: Text,
        },
        Layout {
            // A session time zone behind UTC would shift every date literal if it were consulted.
            name: "array sets, timestamptz dates and native enums",
            options: PostgresOptions::default().sets(SetStorage::Array),
            ddl: "SET TIME ZONE 'America/Los_Angeles';
            CREATE TYPE ticket_status AS ENUM ('closed', 'open');
            CREATE TYPE ticket_priority AS ENUM ('urgent', 'high', 'normal', 'low');
            CREATE TABLE tickets (
                id bigint PRIMARY KEY, title varchar(200) NOT NULL, body text, status ticket_status NOT NULL,
                priority ticket_priority NOT NULL, labels text[] NOT NULL, estimate integer, spent numeric(10, 2),
                created timestamptz NOT NULL, closed_at timestamptz, assignee text, story text NOT NULL,
                archived smallint NOT NULL
            )",
            array_labels: true,
            created: Timestamptz,
            closed: Timestamptz,
        },
        Layout {
            // A date read as midnight in a zone ahead of UTC would land on the previous UTC day.
            name: "date columns",
            options: PostgresOptions::default().dates(Date),
            ddl: "SET TIME ZONE 'Asia/Tokyo';
            CREATE TABLE tickets (
                id integer PRIMARY KEY, title text NOT NULL, body text, status text NOT NULL,
                priority text NOT NULL, labels jsonb NOT NULL, estimate integer, spent integer,
                created date NOT NULL, closed_at date, assignee text, story text NOT NULL,
                archived integer NOT NULL
            )",
            array_labels: false,
            created: Date,
            closed: Date,
        },
        Layout {
            // Each field follows its own storage, and `closed>created` compares across the two.
            name: "per-field storage",
            options: PostgresOptions::default()
                .dates(Date)
                .date_column("created", Timestamptz)
                .set_column("labels", SetStorage::Array),
            ddl: "SET TIME ZONE 'Asia/Tokyo';
            CREATE TABLE tickets (
                id integer PRIMARY KEY, title text NOT NULL, body text, status text NOT NULL,
                priority text NOT NULL, labels text[] NOT NULL, estimate integer, spent integer,
                created timestamptz NOT NULL, closed_at date, assignee text, story text NOT NULL,
                archived integer NOT NULL
            )",
            array_labels: true,
            created: Timestamptz,
            closed: Date,
        },
    ]
}

fn database_url() -> Option<String> {
    let url = std::env::var("DATABASE_URL").unwrap_or_default();
    if url.is_empty() {
        eprintln!("skipped: DATABASE_URL is not set");
        return None;
    }
    Some(url)
}

#[test]
fn every_layout_passes_the_store_checks() {
    let Some(url) = database_url() else { return };
    for (index, layout) in layouts().iter().enumerate() {
        let schema = format!("indexer_test_{index}");
        // One connection, because the schema and time zone are settings of the session.
        let mut client = Client::connect(&url, NoTls).expect("a connection to DATABASE_URL");
        let setup = format!(
            "DROP SCHEMA IF EXISTS {schema} CASCADE; CREATE SCHEMA {schema}; SET search_path TO {schema}; {}",
            layout.ddl
        );
        client.batch_execute(&setup).expect(layout.name);

        let labels = |members: &[&str]| {
            literal(Some(if layout.array_labels {
                format!("{{{}}}", members.join(","))
            } else {
                json!(members).to_string()
            }))
        };
        let date = |field: &str, iso: &str| {
            stored(iso, if field == "created" { layout.created } else { layout.closed })
        };
        let insert = |row: &indexer::Row| insert_sql(row, &labels, &date);
        for row in rows() {
            client.batch_execute(&insert(&row)).expect(layout.name);
        }

        let backend = postgres_backend(&tickets(), client, layout.options.clone()).unwrap();
        let execute = |sql: &str| backend.runner().with(|client| client.batch_execute(sql).expect(sql));
        check_store(&Store {
            name: format!("postgres, {}", layout.name),
            backend: &backend,
            created: returned(layout.created),
            closed: returned(layout.closed),
            regex: true,
            thorough: true,
            execute: &execute,
            insert: &insert,
        });
        execute(&format!("DROP SCHEMA {schema} CASCADE"));
    }
}

#[test]
fn a_storage_must_fit_the_field_it_is_given_to() {
    // No server is needed to refuse a bad option, but one is to build a client.
    let Some(url) = database_url() else { return };
    let connect = || Client::connect(&url, NoTls).unwrap();
    for options in [
        PostgresOptions::default().date_column("title", DateStorage::Date),
        PostgresOptions::default().date_column("labels", DateStorage::Date),
        PostgresOptions::default().set_column("closed", SetStorage::Array),
        PostgresOptions::default().set_column("nope", SetStorage::Array),
    ] {
        assert!(postgres_backend(&tickets(), connect(), options).is_err());
    }
}
