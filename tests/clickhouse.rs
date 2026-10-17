//! These run against a real ClickHouse server and are skipped unless CLICKHOUSE_URL
//! names one. `.cargo/config.toml` points it at the local container.

mod common;

use common::suite::{Returned, Store, check_store, insert_sql};
use common::{rows, tickets};
use indexer::backend::clickhouse::{ClickHouseClient, ClickHouseOptions, DateStorage, clickhouse_backend};

/// One way the tickets can be laid out in ClickHouse.
struct Layout {
    name: &'static str,
    options: ClickHouseOptions,
    ddl: &'static str,
    created: DateStorage,
    closed: DateStorage,
}

/// What is stored for a date of the example rows. An instant is written with its UTC
/// offset, so the time zone a column is declared with has no say in what it means.
fn stored(iso: &str, storage: DateStorage) -> String {
    match storage {
        DateStorage::Text => iso.to_string(),
        DateStorage::Date => iso[..10].to_string(),
        DateStorage::DateTime if iso.len() == 10 => format!("{iso}T00:00:00Z"),
        DateStorage::DateTime => iso.to_string(),
    }
}

fn returned(storage: DateStorage) -> Returned {
    match storage {
        DateStorage::Text => Returned::AsWritten,
        DateStorage::Date => Returned::Day,
        DateStorage::DateTime => Returned::Micros,
    }
}

fn layouts() -> Vec<Layout> {
    use DateStorage::{Date, DateTime, Text};
    vec![
        Layout {
            name: "text dates",
            options: ClickHouseOptions::default().dates(Text),
            ddl: "CREATE TABLE tickets (
                id Int32, title String, body Nullable(String), status String, priority String,
                labels Array(String), estimate Nullable(Float64), spent Nullable(Float64),
                created String, closed_at Nullable(String), assignee Nullable(String), story String,
                archived Int32
            ) ENGINE = MergeTree ORDER BY id",
            created: Text,
            closed: Text,
        },
        Layout {
            // The enums are declared out of rank order, so sorting cannot lean on them.
            name: "datetime64 dates, enums and decimals",
            options: ClickHouseOptions::default(),
            ddl: "CREATE TABLE tickets (
                id Int64, title String, body Nullable(String), status Enum8('closed' = 1, 'open' = 2),
                priority Enum8('urgent' = 1, 'high' = 2, 'normal' = 3, 'low' = 4),
                labels Array(LowCardinality(String)), estimate Nullable(Int32), spent Nullable(Decimal(10, 2)),
                created DateTime64(6, 'UTC'), closed_at Nullable(DateTime64(6, 'UTC')),
                assignee LowCardinality(Nullable(String)), story LowCardinality(String), archived Int8
            ) ENGINE = MergeTree ORDER BY id",
            created: DateTime,
            closed: DateTime,
        },
        Layout {
            // Columns declared in zones either side of UTC would shift every date if
            // the zone were consulted.
            name: "datetime64 dates in other time zones",
            options: ClickHouseOptions::default(),
            ddl: "CREATE TABLE tickets (
                id Int32, title String, body Nullable(String), status String, priority String,
                labels Array(String), estimate Nullable(Int32), spent Nullable(Int32),
                created DateTime64(6, 'America/Los_Angeles'), closed_at Nullable(DateTime64(6, 'Asia/Tokyo')),
                assignee Nullable(String), story String, archived Int32
            ) ENGINE = MergeTree ORDER BY id",
            created: DateTime,
            closed: DateTime,
        },
        Layout {
            name: "date columns",
            options: ClickHouseOptions::default().dates(Date),
            ddl: "CREATE TABLE tickets (
                id Int32, title String, body Nullable(String), status String, priority String,
                labels Array(String), estimate Nullable(Int32), spent Nullable(Int32),
                created Date32, closed_at Nullable(Date), assignee Nullable(String), story String,
                archived Int32
            ) ENGINE = MergeTree ORDER BY id",
            created: Date,
            closed: Date,
        },
        Layout {
            // Each field follows its own storage, and `closed>created` compares across the two.
            name: "per-field storage",
            options: ClickHouseOptions::default().dates(Date).date_column("created", DateTime),
            ddl: "CREATE TABLE tickets (
                id Int32, title String, body Nullable(String), status String, priority String,
                labels Array(String), estimate Nullable(Int32), spent Nullable(Int32),
                created DateTime64(6, 'Asia/Tokyo'), closed_at Nullable(Date), assignee Nullable(String),
                story String, archived Int32
            ) ENGINE = MergeTree ORDER BY id",
            created: DateTime,
            closed: Date,
        },
        Layout {
            name: "text against datetime64",
            options: ClickHouseOptions::default().dates(Text).date_column("created", DateTime),
            ddl: "CREATE TABLE tickets (
                id Int32, title String, body Nullable(String), status String, priority String,
                labels Array(String), estimate Nullable(Int32), spent Nullable(Int32),
                created DateTime64(6, 'UTC'), closed_at Nullable(String), assignee Nullable(String),
                story String, archived Int32
            ) ENGINE = MergeTree ORDER BY id",
            created: DateTime,
            closed: Text,
        },
    ]
}

fn server() -> Option<ClickHouseClient> {
    let url = std::env::var("CLICKHOUSE_URL").unwrap_or_default();
    if url.is_empty() {
        eprintln!("skipped: CLICKHOUSE_URL is not set");
        return None;
    }
    let variable = |name: &str| std::env::var(name).unwrap_or_default();
    Some(
        ClickHouseClient::new(url)
            .user(variable("CLICKHOUSE_USER"))
            .password(variable("CLICKHOUSE_PASSWORD")),
    )
}

#[test]
fn every_layout_passes_the_store_checks() {
    // Each layout has a database of its own, so they can all be checked at once.
    let layouts = layouts();
    std::thread::scope(|scope| {
        for (index, layout) in layouts.iter().enumerate() {
            scope.spawn(move || {
                let Some(server) = server() else { return };
                let database = format!("indexer_test_{index}");
                let admin = |sql: &str| server.execute(sql).unwrap_or_else(|error| panic!("{sql}: {error}"));
                admin(&format!("DROP DATABASE IF EXISTS {database}"));
                admin(&format!("CREATE DATABASE {database}"));

                let client = server.clone().database(&database);
                let execute =
                    |sql: &str| client.execute(sql).unwrap_or_else(|error| panic!("{sql}: {error}"));
                execute(layout.ddl);

                let labels = |members: &[&str]| {
                    let quoted: Vec<String> = members.iter().map(|member| format!("'{member}'")).collect();
                    format!("[{}]", quoted.join(", "))
                };
                let date = |field: &str, iso: &str| {
                    stored(iso, if field == "created" { layout.created } else { layout.closed })
                };
                // Best-effort parsing is what lets a value carry its own UTC offset.
                let insert = |row: &indexer::Row| {
                    insert_sql(row, &labels, &date).replacen(
                        "INSERT INTO tickets VALUES",
                        "INSERT INTO tickets SETTINGS date_time_input_format = 'best_effort' VALUES",
                        1,
                    )
                };
                for row in rows() {
                    execute(&insert(&row));
                }

                let backend = clickhouse_backend(&tickets(), client.clone(), layout.options.clone()).unwrap();
                check_store(&Store {
                    name: format!("clickhouse, {}", layout.name),
                    backend: &backend,
                    created: returned(layout.created),
                    closed: returned(layout.closed),
                    regex: true,
                    thorough: true,
                    execute: &execute,
                    insert: &insert,
                });
                admin(&format!("DROP DATABASE {database}"));
            });
        }
    });
}

#[test]
fn a_storage_must_fit_the_field_it_is_given_to() {
    let options = ClickHouseOptions::default().date_column("title", DateStorage::Date);
    assert!(clickhouse_backend(&tickets(), ClickHouseClient::new("http://localhost:8123"), options).is_err());
}
