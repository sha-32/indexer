//! BigQuery is checked in two places, and neither runs unless asked for.
//!
//! The open-source emulator runs when BIGQUERY_EMULATOR_URL names one, which
//! `.cargo/config.toml` does for the local container. It is free and local, but it is
//! not the real service.
//!
//! The real service runs when BIGQUERY_PROJECT and BIGQUERY_DATASET are set, and takes
//! its access token from the signed-in `gcloud` CLI. It is billed by the query, so it
//! is left out of a plain `cargo test`:
//!
//! ```text
//! BIGQUERY_PROJECT=my-project BIGQUERY_DATASET=indexer_test cargo test --test bigquery real
//! ```

mod common;

use std::process::Command;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use common::suite::{Returned, Store, check_store, insert_sql};
use common::{rows, tickets};
use indexer::backend::bigquery::{BigQueryClient, BigQueryOptions, DateStorage, bigquery_backend};

/// One way the tickets can be laid out in BigQuery.
struct Layout {
    name: &'static str,
    options: BigQueryOptions,
    /// The columns whose types vary between layouts.
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
        DateStorage::Datetime => naive(),
        DateStorage::Timestamp => format!("{}+00", naive()),
    }
}

fn returned(storage: DateStorage) -> Returned {
    match storage {
        DateStorage::Text => Returned::AsWritten,
        DateStorage::Date => Returned::Day,
        DateStorage::Timestamp | DateStorage::Datetime => Returned::Micros,
    }
}

const COLUMNS: &str =
    "id INT64, title STRING, body STRING, status STRING, priority STRING, labels ARRAY<STRING>";

fn layouts() -> Vec<Layout> {
    use DateStorage::{Date, Datetime, Text, Timestamp};
    vec![
        Layout {
            name: "timestamp dates and numerics",
            options: BigQueryOptions::default(),
            ddl: "estimate INT64, spent NUMERIC, created TIMESTAMP, closed_at TIMESTAMP",
            created: Timestamp,
            closed: Timestamp,
        },
        Layout {
            // Each field follows its own storage, and `closed>created` compares across the two.
            name: "per-field storage",
            options: BigQueryOptions::default().dates(Date).date_column("created", Timestamp),
            ddl: "estimate INT64, spent INT64, created TIMESTAMP, closed_at DATE",
            created: Timestamp,
            closed: Date,
        },
        Layout {
            name: "text dates",
            options: BigQueryOptions::default().dates(Text),
            ddl: "estimate FLOAT64, spent FLOAT64, created STRING, closed_at STRING",
            created: Text,
            closed: Text,
        },
        Layout {
            name: "datetime dates holding UTC",
            options: BigQueryOptions::default().dates(Datetime),
            ddl: "estimate INT64, spent INT64, created DATETIME, closed_at DATETIME",
            created: Datetime,
            closed: Datetime,
        },
        Layout {
            name: "date columns",
            options: BigQueryOptions::default().dates(Date),
            ddl: "estimate INT64, spent INT64, created DATE, closed_at DATE",
            created: Date,
            closed: Date,
        },
        Layout {
            name: "text against timestamp",
            options: BigQueryOptions::default().dates(Text).date_column("created", Timestamp),
            ddl: "estimate INT64, spent INT64, created TIMESTAMP, closed_at STRING",
            created: Timestamp,
            closed: Text,
        },
    ]
}

/// Loads the tickets into a table of the layout's shape and runs the store checks on it.
/// The table is always named in full: the emulator does not honour a default dataset.
fn check_layout(engine: &str, client: &BigQueryClient, table: &str, layout: &Layout) {
    let execute = |sql: &str| client.execute(sql).unwrap_or_else(|error| panic!("{sql}: {error}"));
    execute(&format!(
        "CREATE TABLE {table} ({COLUMNS}, {}, assignee STRING, story STRING, archived INT64)",
        layout.ddl
    ));

    let labels = |members: &[&str]| {
        let quoted: Vec<String> = members.iter().map(|member| format!("'{member}'")).collect();
        format!("[{}]", quoted.join(", "))
    };
    let date =
        |field: &str, iso: &str| stored(iso, if field == "created" { layout.created } else { layout.closed });
    let insert = |row: &indexer::Row| insert_sql(row, &labels, &date).replacen("tickets", table, 1);
    // One statement for all the rows: the real service runs each as a job of its own.
    let values: Vec<String> = rows()
        .iter()
        .map(|row| insert(row).split_once(" VALUES ").expect("an insert has values").1.to_string())
        .collect();
    execute(&format!("INSERT INTO {table} VALUES {}", values.join(", ")));

    let backend = bigquery_backend(&tickets(), client.clone(), layout.options.clone()).unwrap().table(table);
    check_store(&Store {
        name: format!("{engine}, {}", layout.name),
        backend: &backend,
        created: returned(layout.created),
        closed: returned(layout.closed),
        regex: true,
        thorough: false,
        execute: &execute,
        insert: &insert,
    });
}

#[test]
fn every_layout_passes_the_store_checks_on_the_emulator() {
    let url = std::env::var("BIGQUERY_EMULATOR_URL").unwrap_or_default();
    if url.is_empty() {
        eprintln!("skipped: BIGQUERY_EMULATOR_URL is not set");
        return;
    }
    // The emulator cannot drop a dataset, so each run makes its own.
    let layouts = layouts();
    let run = std::process::id();
    std::thread::scope(|scope| {
        for (index, layout) in layouts.iter().enumerate() {
            let client = BigQueryClient::new("indexer").endpoint(&url);
            scope.spawn(move || {
                let dataset = format!("indexer_test_{run}_{index}");
                client.create_dataset(&dataset).unwrap_or_else(|error| panic!("{dataset}: {error}"));
                check_layout("bigquery emulator", &client, &format!("{dataset}.tickets"), layout);
            });
        }
    });
}

/// An access token from the signed-in `gcloud` CLI, kept until it is close to expiring.
fn gcloud_token() -> Result<String, String> {
    static CACHED: Mutex<Option<(Instant, String)>> = Mutex::new(None);
    let mut cached = CACHED.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    if let Some((fetched, token)) = cached.as_ref()
        && fetched.elapsed() < Duration::from_secs(30 * 60)
    {
        return Ok(token.clone());
    }
    let program = if cfg!(windows) { "gcloud.cmd" } else { "gcloud" };
    let output = Command::new(program)
        .args(["auth", "print-access-token"])
        .output()
        .map_err(|error| format!("could not run gcloud: {error}"))?;
    if !output.status.success() {
        return Err(String::from_utf8_lossy(&output.stderr).trim().to_string());
    }
    let token = String::from_utf8_lossy(&output.stdout).trim().to_string();
    *cached = Some((Instant::now(), token.clone()));
    Ok(token)
}

/// Drops a table when it goes out of scope, whether the checks passed or panicked.
struct Dropped<'a>(&'a BigQueryClient, &'a str);

impl Drop for Dropped<'_> {
    fn drop(&mut self) {
        let _ = self.0.execute(&format!("DROP TABLE IF EXISTS {}", self.1));
    }
}

#[test]
fn the_real_service_passes_the_store_checks() {
    let variable = |name: &str| std::env::var(name).unwrap_or_default();
    let (project, dataset) = (variable("BIGQUERY_PROJECT"), variable("BIGQUERY_DATASET"));
    if project.is_empty() || dataset.is_empty() {
        eprintln!("skipped: BIGQUERY_PROJECT and BIGQUERY_DATASET are not both set");
        return;
    }
    let mut client = BigQueryClient::new(&project).dataset(&dataset).token(gcloud_token);
    if !variable("BIGQUERY_LOCATION").is_empty() {
        client = client.location(variable("BIGQUERY_LOCATION"));
    }

    // The first three layouts cover every storage but DATETIME, and keep the bill and
    // the wait small.
    let layouts = layouts();
    let run = std::process::id();
    std::thread::scope(|scope| {
        for (index, layout) in layouts.iter().take(3).enumerate() {
            let client = client.clone();
            let table = format!("{dataset}.tickets_{run}_{index}");
            scope.spawn(move || {
                let _dropped = Dropped(&client, &table);
                check_layout("bigquery", &client, &table, layout);
            });
        }
    });
}

#[test]
fn a_storage_must_fit_the_field_it_is_given_to() {
    let options = BigQueryOptions::default().date_column("title", DateStorage::Date);
    assert!(bigquery_backend(&tickets(), BigQueryClient::new("indexer"), options).is_err());
}
