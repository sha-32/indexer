//! DuckDB runs inside the test process, so these need no server.

mod common;

use common::suite::{Returned, Store, check_store, insert_sql, literal};
use common::{ids, query, rows, tickets};
use duckdb::Connection;
use indexer::backend::duckdb::{DateStorage, DuckDbOptions, duckdb_backend};
use indexer::{Backend, PageRequest};
use serde_json::json;

/// One way the tickets can be laid out in DuckDB.
struct Layout {
    name: &'static str,
    options: DuckDbOptions,
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
        DateStorage::Timestamp => naive(),
        DateStorage::Timestamptz => format!("{}+00", naive()),
    }
}

fn returned(storage: DateStorage) -> Returned {
    match storage {
        DateStorage::Text => Returned::AsWritten,
        DateStorage::Date => Returned::Day,
        DateStorage::Timestamp | DateStorage::Timestamptz => Returned::Micros,
    }
}

fn layouts() -> Vec<Layout> {
    use DateStorage::{Date, Text, Timestamp, Timestamptz};
    vec![
        Layout {
            name: "text dates",
            options: DuckDbOptions::default().dates(Text),
            ddl: "CREATE TABLE tickets (
                id INTEGER PRIMARY KEY, title VARCHAR NOT NULL, body VARCHAR, status VARCHAR NOT NULL,
                priority VARCHAR NOT NULL, labels VARCHAR[] NOT NULL, estimate DOUBLE, spent DOUBLE,
                created VARCHAR NOT NULL, closed_at VARCHAR, assignee VARCHAR, story VARCHAR NOT NULL,
                archived INTEGER NOT NULL
            )",
            created: Text,
            closed: Text,
        },
        Layout {
            name: "timestamps holding UTC",
            options: DuckDbOptions::default(),
            ddl: "CREATE TABLE tickets (
                id INTEGER PRIMARY KEY, title VARCHAR NOT NULL, body VARCHAR, status VARCHAR NOT NULL,
                priority VARCHAR NOT NULL, labels VARCHAR[] NOT NULL, estimate INTEGER, spent INTEGER,
                created TIMESTAMP NOT NULL, closed_at TIMESTAMP, assignee VARCHAR, story VARCHAR NOT NULL,
                archived INTEGER NOT NULL
            )",
            created: Timestamp,
            closed: Timestamp,
        },
        Layout {
            // The enums are declared out of rank order, so sorting cannot lean on them.
            name: "timestamptz dates and native enums",
            options: DuckDbOptions::default().dates(Timestamptz),
            ddl: "SET TimeZone = 'America/Los_Angeles';
            CREATE TYPE ticket_status AS ENUM ('closed', 'open');
            CREATE TYPE ticket_priority AS ENUM ('urgent', 'high', 'normal', 'low');
            CREATE TABLE tickets (
                id BIGINT PRIMARY KEY, title VARCHAR NOT NULL, body VARCHAR, status ticket_status NOT NULL,
                priority ticket_priority NOT NULL, labels VARCHAR[] NOT NULL, estimate INTEGER,
                spent DECIMAL(10, 2), created TIMESTAMPTZ NOT NULL, closed_at TIMESTAMPTZ,
                assignee VARCHAR, story VARCHAR NOT NULL, archived SMALLINT NOT NULL
            )",
            created: Timestamptz,
            closed: Timestamptz,
        },
        Layout {
            name: "date columns",
            options: DuckDbOptions::default().dates(Date),
            ddl: "CREATE TABLE tickets (
                id INTEGER PRIMARY KEY, title VARCHAR NOT NULL, body VARCHAR, status VARCHAR NOT NULL,
                priority VARCHAR NOT NULL, labels VARCHAR[] NOT NULL, estimate INTEGER, spent INTEGER,
                created DATE NOT NULL, closed_at DATE, assignee VARCHAR, story VARCHAR NOT NULL,
                archived INTEGER NOT NULL
            )",
            created: Date,
            closed: Date,
        },
        Layout {
            // Each field follows its own storage, and `closed>created` compares across the two.
            name: "per-field storage",
            options: DuckDbOptions::default().dates(Date).date_column("created", Timestamptz),
            ddl: "SET TimeZone = 'Asia/Tokyo';
            CREATE TABLE tickets (
                id INTEGER PRIMARY KEY, title VARCHAR NOT NULL, body VARCHAR, status VARCHAR NOT NULL,
                priority VARCHAR NOT NULL, labels VARCHAR[] NOT NULL, estimate INTEGER, spent INTEGER,
                created TIMESTAMPTZ NOT NULL, closed_at DATE, assignee VARCHAR, story VARCHAR NOT NULL,
                archived INTEGER NOT NULL
            )",
            created: Timestamptz,
            closed: Date,
        },
        Layout {
            name: "text against timestamp",
            options: DuckDbOptions::default().dates(Text).date_column("created", Timestamp),
            ddl: "CREATE TABLE tickets (
                id INTEGER PRIMARY KEY, title VARCHAR NOT NULL, body VARCHAR, status VARCHAR NOT NULL,
                priority VARCHAR NOT NULL, labels VARCHAR[] NOT NULL, estimate INTEGER, spent INTEGER,
                created TIMESTAMP NOT NULL, closed_at VARCHAR, assignee VARCHAR, story VARCHAR NOT NULL,
                archived INTEGER NOT NULL
            )",
            created: Timestamp,
            closed: Text,
        },
    ]
}

fn insert_for(layout: &Layout) -> impl Fn(&indexer::Row) -> String + '_ {
    move |row| {
        let labels = |members: &[&str]| literal(Some(json!(members).to_string()));
        let date = |field: &str, iso: &str| {
            stored(iso, if field == "created" { layout.created } else { layout.closed })
        };
        insert_sql(row, &labels, &date)
    }
}

#[test]
fn every_layout_passes_the_store_checks() {
    for layout in layouts() {
        let connection = Connection::open_in_memory().expect("an in-memory database");
        connection.execute_batch(layout.ddl).unwrap_or_else(|error| panic!("{}: {error}", layout.name));
        let insert = insert_for(&layout);
        for row in rows() {
            let sql = insert(&row);
            connection.execute_batch(&sql).unwrap_or_else(|error| panic!("{sql}: {error}"));
        }

        let backend = duckdb_backend(&tickets(), connection, layout.options.clone()).unwrap();
        let execute = |sql: &str| {
            backend
                .runner()
                .with(|connection| connection.execute_batch(sql).unwrap_or_else(|e| panic!("{sql}: {e}")))
        };
        check_store(&Store {
            name: format!("duckdb, {}", layout.name),
            backend: &backend,
            created: returned(layout.created),
            closed: returned(layout.closed),
            regex: true,
            thorough: true,
            execute: &execute,
            insert: &insert,
        });
    }
}

#[test]
fn a_file_is_searched_through_a_view_named_after_the_collection() {
    let layout = &layouts()[1];
    let file = std::env::temp_dir().join(format!("indexer-tickets-{}.csv", std::process::id()));
    let path = file.to_string_lossy().replace('\\', "/").replace('\'', "''");

    // Write the tickets out to a CSV file, then search the file and nothing else.
    let writer = Connection::open_in_memory().unwrap();
    writer.execute_batch(layout.ddl).unwrap();
    let insert = insert_for(layout);
    for row in rows() {
        writer.execute_batch(&insert(&row)).unwrap();
    }
    writer.execute_batch(&format!("COPY tickets TO '{path}' (HEADER)")).unwrap();
    drop(writer);

    let reader = Connection::open_in_memory().unwrap();
    let columns = "{'id': 'INTEGER', 'title': 'VARCHAR', 'body': 'VARCHAR', 'status': 'VARCHAR', \
                   'priority': 'VARCHAR', 'labels': 'VARCHAR[]', 'estimate': 'INTEGER', 'spent': 'INTEGER', \
                   'created': 'TIMESTAMP', 'closed_at': 'TIMESTAMP', 'assignee': 'VARCHAR', \
                   'story': 'VARCHAR', 'archived': 'INTEGER'}";
    reader
        .execute_batch(&format!(
            "CREATE VIEW tickets AS SELECT * FROM read_csv('{path}', header = true, columns = {columns})"
        ))
        .unwrap();
    let backend = duckdb_backend(&tickets(), reader, layout.options.clone()).unwrap();
    let tickets = tickets();
    for (input, expected) in [
        ("p>=high l:bug or is:overdue order:created", vec![5, 1, 7]),
        ("closed>created", vec![3, 7]),
        ("l<eng unique:story prefer:newest", vec![1, 4, 7, 8]),
    ] {
        let page = backend.search(&query(&tickets, input), &PageRequest::first(100)).unwrap();
        assert_eq!(ids(&page.rows), expected, "{input}");
    }
    let _ = std::fs::remove_file(file);
}

#[test]
fn a_storage_must_fit_the_field_it_is_given_to() {
    let options = DuckDbOptions::default().date_column("title", DateStorage::Date);
    assert!(duckdb_backend(&tickets(), Connection::open_in_memory().unwrap(), options).is_err());
}
