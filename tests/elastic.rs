//! These run against real OpenSearch and Elasticsearch servers, and each is skipped
//! unless its variable (OPENSEARCH_URL, ELASTICSEARCH_URL) names one.
//! `.cargo/config.toml` points them at the local containers.

mod common;

use common::suite::{Returned, Store, check_store};
use common::{ids, query, rows, tickets};
use indexer::backend::elastic::{DateStorage, ElasticClient, ElasticOptions, elastic_backend};
use indexer::{Backend, Error, PageRequest, Row};
use serde_json::{Value as Json, json};

/// One way the tickets can be mapped in an index.
struct Layout {
    name: &'static str,
    options: ElasticOptions,
    /// The mapping type of `title`, and of the two date fields.
    title: Json,
    created: (DateStorage, Json),
    closed: (DateStorage, Json),
}

fn returned(storage: DateStorage) -> Returned {
    match storage {
        DateStorage::Text => Returned::AsWritten,
        DateStorage::Day => Returned::Day,
        DateStorage::Instant => Returned::Micros,
    }
}

fn layouts(engine: &str) -> Vec<Layout> {
    use DateStorage::{Day, Instant, Text};
    let keyword = || json!({ "type": "keyword" });
    let nanos = || json!({ "type": "date_nanos" });
    let day = || json!({ "type": "date", "format": "yyyy-MM-dd" });
    vec![
        Layout {
            // OpenSearch cannot run a range over a `wildcard` field, which paging in
            // title order needs, so only Elasticsearch gets one here.
            name: "nanosecond dates and a wildcard title",
            options: ElasticOptions::default(),
            title: if engine == "elasticsearch" { json!({ "type": "wildcard" }) } else { keyword() },
            created: (Instant, nanos()),
            closed: (Instant, nanos()),
        },
        Layout {
            name: "calendar days",
            options: ElasticOptions::default().dates(Day),
            title: keyword(),
            created: (Day, day()),
            closed: (Day, day()),
        },
        Layout {
            name: "text dates",
            options: ElasticOptions::default().dates(Text),
            title: keyword(),
            created: (Text, keyword()),
            closed: (Text, keyword()),
        },
        Layout {
            // Each field follows its own mapping, and `closed>created` compares across the two.
            name: "per-field mapping",
            options: ElasticOptions::default().dates(Day).date_column("created", Instant),
            title: keyword(),
            created: (Instant, nanos()),
            closed: (Day, day()),
        },
    ]
}

/// A row as the document that is indexed for it.
fn document(row: &Row, layout: &Layout) -> String {
    let mut document = row.clone();
    let date = |value: &Json, storage: DateStorage| match (value.as_str(), storage) {
        (Some(iso), DateStorage::Day) => json!(iso[..10]),
        _ => value.clone(),
    };
    document["created"] = date(&row["created"], layout.created.0);
    let closed = date(&row["closed"], layout.closed.0);
    document.remove("closed");
    document.insert("closed_at".into(), closed);
    Json::Object(document).to_string()
}

fn check_server(variable: &str, engine: &str) {
    let url = std::env::var(variable).unwrap_or_default();
    if url.is_empty() {
        eprintln!("skipped: {variable} is not set");
        return;
    }
    // Each layout has an index of its own, so they can all be checked at once.
    let layouts = layouts(engine);
    std::thread::scope(|scope| {
        for (number, layout) in layouts.iter().enumerate() {
            let client = ElasticClient::new(&url);
            scope.spawn(move || {
                let index = format!("indexer_test_{number}");
                let keyword = json!({ "type": "keyword" });
                let mapping = json!({ "mappings": { "properties": {
                    "id": { "type": "long" },
                    "title": layout.title,
                    "body": keyword,
                    "status": keyword,
                    "priority": keyword,
                    "labels": keyword,
                    "estimate": { "type": "double" },
                    "spent": { "type": "long" },
                    "created": layout.created.1,
                    "closed_at": layout.closed.1,
                    "assignee": keyword,
                    "story": keyword,
                    "archived": { "type": "integer" },
                } } });
                let _ = client.request("DELETE", &index, None);
                client
                    .request("PUT", &index, Some(&mapping))
                    .unwrap_or_else(|error| panic!("{index}: {error}"));

                // The store checks hand over what `insert` returns, which here is a
                // document, and it is searchable as soon as the request returns.
                let execute = |document: &str| {
                    let document: Json = serde_json::from_str(document).expect("a document");
                    client
                        .request("POST", &format!("{index}/_doc?refresh=true"), Some(&document))
                        .unwrap_or_else(|error| panic!("{document}: {error}"));
                };
                let insert = |row: &Row| document(row, layout);
                for row in rows() {
                    execute(&insert(&row));
                }

                let tickets = tickets();
                let backend =
                    elastic_backend(&tickets, client.clone(), layout.options.clone()).unwrap().index(&index);
                let name = format!("{engine}, {}", layout.name);
                check_store(&Store {
                    name: name.clone(),
                    backend: &backend,
                    created: returned(layout.created.0),
                    closed: returned(layout.closed.0),
                    regex: false,
                    thorough: true,
                    execute: &execute,
                    insert: &insert,
                });

                // Characters that mean something in a wildcard pattern are matched as themselves.
                execute(&document(&common::rows()[1], layout).replace("Add SSO login", "What? *Really*"));
                for (input, expected) in [("\"? *R\"", vec![2]), ("\"*\"", vec![2]), ("\"?\"", vec![2])] {
                    let found = backend.search(&query(&tickets, input), &PageRequest::first(10)).unwrap();
                    let fresh: Vec<i64> = ids(&found.rows).into_iter().filter(|id| *id == 2).collect();
                    assert_eq!(
                        fresh.first().copied().into_iter().collect::<Vec<_>>(),
                        expected,
                        "{name}: {input}"
                    );
                    assert!(found.total <= 2, "{name}: {input} matched {} rows", found.total);
                }

                // A rollup with more groups than allowed is refused, not cut short.
                let few = elastic_backend(&tickets, client.clone(), layout.options.clone().max_groups(3))
                    .unwrap()
                    .index(&index);
                match few.search(&query(&tickets, "unique:story"), &PageRequest::first(10)) {
                    Err(Error::Query(error)) => assert_eq!(error.issues[0].code, "too_many_groups", "{name}"),
                    other => panic!("{name}: a rollup past the limit gave {other:?}"),
                }
                client.request("DELETE", &index, None).unwrap();
            });
        }
    });
}

#[test]
fn opensearch_passes_the_store_checks_in_every_layout() {
    check_server("OPENSEARCH_URL", "opensearch");
}

#[test]
fn elasticsearch_passes_the_store_checks_in_every_layout() {
    check_server("ELASTICSEARCH_URL", "elasticsearch");
}

#[test]
fn a_mapping_must_fit_the_field_it_is_given_to() {
    let options = ElasticOptions::default().date_column("title", DateStorage::Day);
    assert!(elastic_backend(&tickets(), ElasticClient::new("http://localhost:9200"), options).is_err());
}
