mod common;

use common::{rows, tickets};
use indexer::syntax::{describe, example_queries, guide};
use indexer::{Backend, Collection, Field, MemoryBackend, PageRequest, ParseOptions, parse};
use serde_json::{Value as Json, json};

fn named<'a>(description: &'a Json, section: &str, name: &str) -> &'a Json {
    description[section]
        .as_array()
        .and_then(|items| items.iter().find(|item| item["name"] == name))
        .unwrap_or_else(|| panic!("no {name} in {section}"))
}

#[test]
fn every_example_is_a_query_that_parses_and_runs() {
    let tickets = tickets();
    let backend = MemoryBackend::new(&tickets, rows());
    let examples = example_queries(&describe(&tickets));
    assert!(examples.len() > 60, "only {} examples", examples.len());
    for example in examples {
        let query = parse(&tickets, &example, &ParseOptions::default())
            .unwrap_or_else(|error| panic!("{example}: {error}"));
        backend.search(&query, &PageRequest::first(5)).unwrap_or_else(|error| panic!("{example}: {error}"));
    }
}

#[test]
fn a_field_is_described_by_what_can_be_written_against_it() {
    let description = describe(&tickets());
    assert_eq!(description["object"], "syntax");
    assert_eq!(description["default_field"], "title");

    let priority = named(&description, "fields", "priority");
    assert_eq!(priority["aliases"], json!(["p"]));
    assert_eq!(priority["kind"], "ordinal");
    assert_eq!(priority["description"], "How soon it needs doing.");
    assert_eq!(priority["values"], json!(["low", "normal", "high", "urgent"]));
    let operators: Vec<&str> =
        priority["operators"].as_array().unwrap().iter().map(|op| op["operator"].as_str().unwrap()).collect();
    assert_eq!(operators, [":", "=", "!=", ">", ">=", "<", "<="]);

    // A text field has no ordering, so it offers no ordering operators.
    let title = named(&description, "fields", "title");
    assert_eq!(title["operators"].as_array().unwrap().len(), 3);
    assert_eq!((&title["default"], &title["regex"]), (&json!(true), &json!(true)));
    assert_eq!(named(&description, "fields", "body")["regex"], false);

    let labels = named(&description, "fields", "labels");
    assert_eq!(labels["nicknames"], json!({ "eng": ["bug", "infra"] }));
    // The backend column name is not part of the vocabulary.
    assert!(!description.to_string().contains("closed_at"));
}

#[test]
fn named_terms_are_described_with_what_they_stand_for() {
    let description = describe(&tickets());
    let overdue = named(&description, "predicates", "overdue");
    assert_eq!(overdue["description"], "Open tickets that have taken longer than estimated.");
    assert_eq!(overdue["means"], "is:open spent>estimate");
    assert_eq!(named(&description, "predicates", "open")["description"], Json::Null);

    let created = named(&description, "orders", "created");
    assert_eq!((&created["natural_direction"], &created["nulls"]), (&json!("desc"), &json!("last")));
    assert_eq!(created["fields"], json!(["created"]));
    assert_eq!(named(&description, "rollups", "story")["by"], "story");
    assert_eq!(named(&description, "preferences", "newest")["keeps"], "highest");
    assert_eq!(named(&description, "exclusions", "archived")["hides"], "is:archived");
}

#[test]
fn the_guide_covers_every_part_of_the_vocabulary() {
    let tickets = tickets();
    let text = guide(&tickets);
    assert!(text.starts_with("# Searching tickets\n"));
    for heading in [
        "## The basics",
        "## Fields",
        "## Named filters",
        "## Sorting",
        "## One row per group",
        "## Choosing the row a group keeps",
        "## Hidden rows",
    ] {
        assert!(text.contains(heading), "missing {heading}");
    }
    for (name, _) in tickets.fields() {
        assert!(text.contains(&format!("### {name}\n")), "missing field {name}");
    }
    assert!(text.contains("- **Also written:** `p`"));
    assert!(text.contains("- **Values, lowest first:** `low`, `normal`, `high`, `urgent`"));
    assert!(text.contains("| `priority>=high` | priority is high or above |"));
    assert!(text.contains("- **overdue**: Open tickets that have taken longer than estimated. Short for `is:open spent>estimate`."));
    assert!(text.contains("| `unique:story prefer:newest` |"));
}

#[test]
fn a_collection_with_nothing_optional_gets_a_guide_without_those_sections() {
    let plain = Collection::builder("notes")
        .field("id", Field::number())
        .field("text", Field::text())
        .id("id")
        .default_field("text")
        .build()
        .unwrap();
    let text = guide(&plain);
    assert!(text.contains("## Fields") && !text.contains("## Sorting") && !text.contains("## Named filters"));
    for example in example_queries(&describe(&plain)) {
        parse(&plain, &example, &ParseOptions::default())
            .unwrap_or_else(|error| panic!("{example}: {error}"));
    }
}

#[test]
fn a_description_must_have_something_to_describe() {
    let base = || {
        Collection::builder("t")
            .field("id", Field::number())
            .field("title", Field::text())
            .id("id")
            .default_field("title")
            .predicate("done", "id=1")
    };
    assert!(base().describe("IS:Done", "Finished.").build().is_ok());
    for term in ["is:nope", "order:created", "title", "has:title"] {
        let error = base().describe(term, "x").build().unwrap_err().to_string();
        assert!(error.contains("has nothing to describe"), "{term}: {error}");
    }
}
