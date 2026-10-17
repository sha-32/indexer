//! These run against a real MongoDB server and are skipped unless MONGODB_URL names one.
//! `.cargo/config.toml` points it at the local container.

mod common;

use common::suite::{Returned, Store, check_store};
use common::{rows, tickets};
use indexer::Row;
use indexer::backend::mongodb::{DateStorage, MongoOptions, mongo_backend};
use mongodb::bson::{Bson, doc};
use mongodb::sync::Client;
use serde_json::{Value as Json, json};

/// One way the tickets can be stored in MongoDB.
struct Layout {
    name: &'static str,
    options: MongoOptions,
    created: DateStorage,
    closed: DateStorage,
}

fn returned(storage: DateStorage) -> Returned {
    match storage {
        DateStorage::Text => Returned::AsWritten,
        DateStorage::Date => Returned::Millis,
    }
}

fn layouts() -> Vec<Layout> {
    use DateStorage::{Date, Text};
    vec![
        Layout { name: "BSON dates", options: MongoOptions::default(), created: Date, closed: Date },
        Layout {
            name: "text dates",
            options: MongoOptions::default().dates(Text),
            created: Text,
            closed: Text,
        },
        Layout {
            // Each field follows its own storage, and `closed>created` compares across the two.
            name: "per-field storage",
            options: MongoOptions::default().dates(Text).date_column("created", Date),
            created: Date,
            closed: Text,
        },
    ]
}

/// A row as the document stored for it, in extended JSON so a date can say it is one.
fn document(row: &Row, layout: &Layout) -> String {
    let mut document = row.clone();
    let date = |value: &Json, storage: DateStorage| match (value.as_str(), storage) {
        (Some(iso), DateStorage::Date) if iso.len() == 10 => json!({ "$date": format!("{iso}T00:00:00Z") }),
        (Some(iso), DateStorage::Date) => json!({ "$date": iso }),
        _ => value.clone(),
    };
    document["created"] = date(&row["created"], layout.created);
    let closed = date(&row["closed"], layout.closed);
    document.remove("closed");
    document.insert("closed_at".into(), closed);
    Json::Object(document).to_string()
}

#[test]
fn every_layout_passes_the_store_checks() {
    let url = std::env::var("MONGODB_URL").unwrap_or_default();
    if url.is_empty() {
        eprintln!("skipped: MONGODB_URL is not set");
        return;
    }
    // Each layout has a database of its own, so they can all be checked at once.
    let layouts = layouts();
    std::thread::scope(|scope| {
        for (number, layout) in layouts.iter().enumerate() {
            let url = url.clone();
            scope.spawn(move || {
                let client = Client::with_uri_str(&url).expect("a connection to MONGODB_URL");
                let database = client.database(&format!("indexer_test_{number}"));
                database.drop().run().expect("an empty database");

                let tickets = tickets();
                let backend = mongo_backend(&tickets, &database, layout.options.clone()).unwrap();
                // The store checks hand over what `insert` returns, which here is a document.
                let execute = |document: &str| {
                    let parsed: Json = serde_json::from_str(document).expect("a document");
                    let Ok(Bson::Document(parsed)) = Bson::try_from(parsed) else {
                        panic!("{document} is not a document")
                    };
                    backend
                        .documents()
                        .insert_one(parsed)
                        .run()
                        .unwrap_or_else(|error| panic!("{document}: {error}"));
                };
                let insert = |row: &Row| document(row, layout);
                for row in rows() {
                    execute(&insert(&row));
                }
                // A field left out of a document entirely must read the same as one set
                // to null, so the body is removed wherever it was null.
                backend
                    .documents()
                    .update_many(doc! { "body": Bson::Null }, doc! { "$unset": { "body": "" } })
                    .run()
                    .expect("the bodies unset");

                check_store(&Store {
                    name: format!("mongodb, {}", layout.name),
                    backend: &backend,
                    created: returned(layout.created),
                    closed: returned(layout.closed),
                    regex: true,
                    thorough: true,
                    execute: &execute,
                    insert: &insert,
                });
                database.drop().run().expect("the database dropped");
            });
        }
    });
}

#[test]
fn a_storage_must_fit_the_field_it_is_given_to() {
    let url = std::env::var("MONGODB_URL").unwrap_or_default();
    if url.is_empty() {
        return;
    }
    let database = Client::with_uri_str(&url).unwrap().database("indexer_test_options");
    let options = MongoOptions::default().date_column("title", DateStorage::Text);
    assert!(mongo_backend(&tickets(), &database, options).is_err());
}
