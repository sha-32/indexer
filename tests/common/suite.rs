//! The checks every SQL store must pass, whatever its engine and however it lays out
//! the tickets.

use indexer::{Backend, Error, MemoryBackend, PageRequest, Row};
use serde_json::{Value as Json, json};

use super::{CASES, ids, query, rows, tickets, walk};

/// How a store hands back a date of the example rows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Returned {
    /// The ISO text that was stored.
    AsWritten,
    /// `YYYY-MM-DD`: the time of day was dropped on the way in.
    Day,
    /// A UTC timestamp with all six fractional digits.
    Micros,
    /// The same form, from a store that keeps milliseconds: the last three digits are zero.
    Millis,
}

impl Returned {
    pub fn of(self, iso: &str) -> String {
        match self {
            Returned::AsWritten => iso.to_string(),
            Returned::Day => iso[..10].to_string(),
            Returned::Micros if iso.len() == 10 => format!("{iso}T00:00:00.000000Z"),
            Returned::Micros => iso.replace("Z", "000Z"),
            Returned::Millis if iso.len() == 10 => format!("{iso}T00:00:00.000000Z"),
            Returned::Millis => format!("{}000Z", &iso[..23]),
        }
    }
}

/// A store holding the example rows, ready to be checked.
pub struct Store<'a> {
    pub name: String,
    pub backend: &'a dyn Backend,
    pub created: Returned,
    pub closed: Returned,
    /// Whether the store can match regular expressions. One that cannot must refuse them.
    pub regex: bool,
    /// Whether to walk every case by cursor. A store that is slow or billed by the query
    /// walks only the cases that sort or group, which is where a cursor can go wrong.
    pub thorough: bool,
    /// Runs a statement outside the backend.
    pub execute: &'a dyn Fn(&str),
    /// The statement that inserts a row.
    pub insert: &'a dyn Fn(&Row) -> String,
}

/// A value as a SQL string literal, or `NULL`.
pub fn literal(value: Option<String>) -> String {
    value.map_or("NULL".to_string(), |text| format!("'{}'", text.replace('\'', "''")))
}

/// The statement that inserts a row, given the SQL expression the store writes for a set
/// of labels, and the text it stores for a date of a given field.
pub fn insert_sql(
    row: &Row,
    labels: &dyn Fn(&[&str]) -> String,
    date: &dyn Fn(&str, &str) -> String,
) -> String {
    let text = |field: &str| row[field].as_str().map(str::to_string);
    let members: Vec<&str> = row["labels"].as_array().unwrap().iter().filter_map(Json::as_str).collect();
    let values = [
        row["id"].to_string(),
        literal(text("title")),
        literal(text("body")),
        literal(text("status")),
        literal(text("priority")),
        labels(&members),
        row["estimate"].to_string(),
        row["spent"].to_string(),
        literal(text("created").map(|iso| date("created", &iso))),
        literal(text("closed").map(|iso| date("closed", &iso))),
        literal(text("assignee")),
        literal(text("story")),
        row["archived"].to_string(),
    ];
    format!("INSERT INTO tickets VALUES ({})", values.join(", "))
}

/// Runs every check against a store. The later checks add rows, so the order matters.
pub fn check_store(store: &Store) {
    let tickets = tickets();
    let name = &store.name;
    let backend = store.backend;
    let all = PageRequest::first(100);

    // Every case returns the expected rows in order.
    let cases = || CASES.iter().filter(|(input, _)| store.regex || !input.contains(":/"));
    for (input, expected) in cases() {
        let page =
            backend.search(&query(&tickets, input), &all).unwrap_or_else(|e| panic!("{name}: {input}: {e}"));
        assert_eq!(ids(&page.rows), *expected, "{name}: {input}");
        assert_eq!(page.total, expected.len() as u64, "{name}: {input}");
    }

    // Following cursors visits every row once, in order.
    let walked = cases().filter(|(input, _)| {
        store.thorough || input.is_empty() || input.contains("order:") || input.contains("unique:")
    });
    for (input, expected) in walked {
        assert_eq!(walk(backend, &query(&tickets, input), 2), *expected, "{name}: {input} by cursor");
    }

    // A store without regular expressions says so, and does not guess.
    if !store.regex {
        for input in ["title:/^start/", "-title:/x/ p>=high", "l:bug or name:/x/"] {
            match backend.search(&query(&tickets, input), &all) {
                Err(Error::Query(error)) => assert_eq!(error.issues[0].code, "unsupported_regex", "{name}"),
                other => panic!("{name}: {input} gave {other:?}"),
            }
        }
    }

    // Pages are slices of one stable order.
    let page = backend.search(&query(&tickets, "order:priority"), &PageRequest::first(3).offset(2)).unwrap();
    assert_eq!(ids(&page.rows), [4, 2, 9], "{name}");
    assert_eq!((page.total, page.has_more), (9, true), "{name}");

    // Rows decode to the values a row holds.
    let everything = query(&tickets, "include:archived");
    let found = backend.search(&everything, &all).unwrap().rows;
    let mut expected = MemoryBackend::new(&tickets, rows()).search(&everything, &all).unwrap().rows;
    for row in &mut expected {
        for (field, shape) in [("created", store.created), ("closed", store.closed)] {
            if let Some(iso) = row[field].as_str() {
                row[field] = json!(shape.of(iso));
            }
        }
    }
    assert_eq!(found, expected, "{name}");

    // Two date fields compare as instants, however each is stored. This ticket was
    // created at UTC midnight and closed that same day: equal, in every storage.
    let mut same_day = rows().remove(9);
    same_day.insert("id".into(), json!(11));
    same_day.insert("created".into(), json!("2026-05-01"));
    same_day.insert("closed".into(), json!("2026-05-01"));
    (store.execute)(&(store.insert)(&same_day));
    for (input, expected) in [("closed>=created", vec![11]), ("closed<created", vec![])] {
        let parsed = query(&tickets, &format!("id=11 include:archived {input}"));
        assert_eq!(ids(&backend.search(&parsed, &all).unwrap().rows), expected, "{name}: {input}");
    }

    // A cursor is exact to the microsecond. These two tickets are a microsecond apart:
    // closer than a millisecond clock can tell.
    for (id, created) in [(12, "2026-06-01T00:00:00.000001Z"), (13, "2026-06-01T00:00:00.000002Z")] {
        let mut burst = rows().remove(0);
        burst.insert("id".into(), json!(id));
        burst.insert("title".into(), json!("Burst"));
        burst.insert("created".into(), json!(created));
        (store.execute)(&(store.insert)(&burst));
    }
    let bursts = query(&tickets, "burst order:created");
    let both = ids(&backend.search(&bursts, &all).unwrap().rows);
    assert_eq!(both.len(), 2, "{name}");
    assert_eq!(walk(backend, &bursts, 1), both, "{name}: bursts by cursor");
    if store.created == Returned::Micros {
        assert_eq!(both, [13, 12], "{name}");
    }
}
