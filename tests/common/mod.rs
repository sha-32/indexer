//! A small issue tracker: the worked example and the fixture every test runs against.
#![allow(dead_code)]

use indexer::{Backend, Collection, Field, Order, PageRequest, ParseOptions, Query, Row, parse};
use serde_json::{Value as Json, json};

mod cases;
pub mod suite;
#[allow(unused_imports)]
pub use cases::CASES;

/// 2026-03-10T00:00:00Z, the instant `now` means in the case table.
pub const NOW_MS: i64 = 1_773_100_800_000;

pub fn tickets() -> Collection {
    Collection::builder("tickets")
        .field("id", Field::number())
        .field("title", Field::text().alias("name").regex().describe("The one-line summary of the ticket."))
        .field("body", Field::text().nullable())
        .field("status", Field::keyword().values(["open", "closed"]).alias("s"))
        .field(
            "priority",
            Field::ordinal(["low", "normal", "high", "urgent"])
                .alias("p")
                .describe("How soon it needs doing."),
        )
        .field(
            "labels",
            Field::set(["bug", "feature", "docs", "infra"]).alias("l").nickname("eng", ["bug", "infra"]),
        )
        .field("estimate", Field::number().alias("est").nullable())
        .field("spent", Field::number().nullable())
        .field("created", Field::date())
        .field("closed", Field::date().nullable().column("closed_at"))
        .field("assignee", Field::keyword().alias("a").nullable())
        .field("story", Field::keyword())
        .field("archived", Field::number())
        .id("id")
        .default_field("title")
        .predicate("open", "status:open")
        .predicate("overdue", "is:open spent>estimate")
        .predicate("archived", "archived=1")
        .order("created", Order::by(["created"]).desc())
        .order("priority", Order::by(["priority", "created"]).desc())
        .order("estimate", Order::by(["estimate"]))
        .order("title", Order::by(["title"]))
        .rollup("story", "story")
        .preference("newest", "created", true)
        .preference("cheapest", "estimate", false)
        .exclusion("archived", "is:archived")
        .describe("is:overdue", "Open tickets that have taken longer than estimated.")
        .describe("order:created", "Newest first.")
        .describe("unique:story", "Collapses the tickets of a story into one.")
        .describe("prefer:newest", "The most recently created.")
        .describe("include:archived", "Tickets filed away after a year.")
        .build()
        .expect("the example collection is consistent")
}

#[allow(clippy::too_many_arguments)]
fn ticket(
    id: i64,
    title: &str,
    status: &str,
    priority: &str,
    labels: &[&str],
    estimate: Option<i64>,
    spent: Option<i64>,
    created: &str,
    assignee: Option<&str>,
    story: &str,
) -> Row {
    let row = json!({
        "id": id,
        "title": title,
        "body": null,
        "status": status,
        "priority": priority,
        "labels": labels,
        "estimate": estimate,
        "spent": spent,
        "created": created,
        "closed": null,
        "assignee": assignee,
        "story": story,
        "archived": 0,
    });
    match row {
        Json::Object(row) => row,
        _ => unreachable!(),
    }
}

fn with(mut row: Row, field: &str, value: Json) -> Row {
    row.insert(field.to_string(), value);
    row
}

pub fn rows() -> Vec<Row> {
    let kim = Some("kim");
    let lee = Some("lee");
    let ana = Some("ana");
    vec![
        ticket(
            1,
            "Login fails on Safari",
            "open",
            "urgent",
            &["bug"],
            Some(3),
            Some(5),
            "2026-01-05",
            kim,
            "auth",
        ),
        ticket(2, "Add SSO login", "open", "high", &["feature"], Some(8), Some(2), "2026-01-12", lee, "auth"),
        with(
            ticket(
                3,
                "Document the search syntax",
                "closed",
                "low",
                &["docs"],
                Some(2),
                Some(2),
                "2026-02-01",
                None,
                "docs",
            ),
            "closed",
            json!("2026-02-03"),
        ),
        ticket(
            4,
            "Rotate TLS certificates",
            "open",
            "high",
            &["infra"],
            None,
            Some(1),
            "2026-02-14",
            kim,
            "ops",
        ),
        ticket(
            5,
            "Flaky deploy (stage 2)",
            "open",
            "normal",
            &["bug", "infra"],
            Some(5),
            Some(9),
            "2026-03-02",
            ana,
            "ops",
        ),
        ticket(6, "Dark mode", "open", "low", &["feature"], Some(13), None, "2026-03-20", None, "ui"),
        with(
            ticket(
                7,
                "100% CPU in indexer",
                "closed",
                "urgent",
                &["bug"],
                Some(5),
                Some(4),
                "2025-12-28",
                lee,
                "perf",
            ),
            "closed",
            json!("2026-01-02T09:30:00.000Z"),
        ),
        ticket(8, "Untriaged idea", "open", "normal", &[], None, None, "2026-04-01", None, "ui"),
        with(
            ticket(
                9,
                "Rename \"legacy_mode\" flag",
                "open",
                "normal",
                &["infra", "docs"],
                Some(1),
                Some(1),
                "2026-04-09",
                ana,
                "ops",
            ),
            "body",
            json!("Touches config and docs."),
        ),
        with(
            with(
                ticket(
                    10,
                    "Old login redesign",
                    "closed",
                    "normal",
                    &["feature"],
                    Some(21),
                    Some(30),
                    "2025-06-15",
                    kim,
                    "auth",
                ),
                "closed",
                json!("2025-09-01"),
            ),
            "archived",
            json!(1),
        ),
    ]
}

/// Parses a query against the tickets with `now` fixed. Panics if it is invalid.
pub fn query(collection: &Collection, input: &str) -> Query {
    let options = ParseOptions { now_ms: Some(NOW_MS), ..ParseOptions::default() };
    parse(collection, input, &options).unwrap_or_else(|error| panic!("{input}: {error}"))
}

pub fn ids(rows: &[Row]) -> Vec<i64> {
    rows.iter().map(|row| row["id"].as_i64().expect("an integer id")).collect()
}

/// Follows cursors from the first page to the last and returns every row's id in order.
pub fn walk(backend: &dyn Backend, query: &Query, limit: usize) -> Vec<i64> {
    let mut found = Vec::new();
    let mut request = PageRequest::first(limit);
    for _ in 0..1000 {
        let page = backend.search(query, &request).expect("a page");
        found.extend(ids(&page.rows));
        match page.next {
            Some(next) => request = PageRequest::after(limit, next),
            None => return found,
        }
    }
    panic!("the cursor never reached a last page");
}

/// An in-memory SQLite database holding the example rows.
#[cfg(feature = "sqlite")]
pub fn sqlite_database() -> rusqlite::Connection {
    let connection = rusqlite::Connection::open_in_memory().expect("an in-memory database");
    connection
        .execute_batch(
            "CREATE TABLE tickets (
                id INTEGER PRIMARY KEY, title TEXT NOT NULL, body TEXT, status TEXT NOT NULL,
                priority TEXT NOT NULL, labels TEXT NOT NULL, estimate REAL, spent REAL,
                created TEXT NOT NULL, closed_at TEXT, assignee TEXT, story TEXT NOT NULL,
                archived INTEGER NOT NULL
            )",
        )
        .expect("the table");
    for row in rows() {
        let text = |field: &str| row[field].as_str().map(str::to_string);
        connection
            .execute(
                "INSERT INTO tickets VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
                rusqlite::params![
                    row["id"].as_i64(),
                    text("title"),
                    text("body"),
                    text("status"),
                    text("priority"),
                    row["labels"].to_string(),
                    row["estimate"].as_f64(),
                    row["spent"].as_f64(),
                    text("created"),
                    text("closed"),
                    text("assignee"),
                    text("story"),
                    row["archived"].as_i64(),
                ],
            )
            .expect("a row");
    }
    connection
}
