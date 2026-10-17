# indexer

A query language for filtering and sorting rows, in the style of a search box:

```text
p>=high l:bug -a:kim (est>5 or is:overdue) created>=2026-03 order:created
```

You declare a collection once: its fields, their kinds, and any named filters and
sort orders. From that one declaration you get:

- a parser that checks every query against the collection and explains what is wrong
  and where,
- backends that run the same query, with the same results, in memory and against
  nine databases and two search engines,
- stable cursor paging,
- an HTTP responder with a list and error format,
- a generated guide to the collection's syntax for the people who will write queries.

The people writing queries never see table names, column names or which database is
behind it.

## Example

```rust
use indexer::{Backend, Collection, Field, MemoryBackend, Order, PageRequest, ParseOptions, Row, parse};
use serde_json::json;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let tasks = Collection::builder("tasks")
        .field("id", Field::number())
        .field("title", Field::text())
        .field("priority", Field::ordinal(["low", "normal", "high"]).alias("p"))
        .field("labels", Field::set(["bug", "feature", "docs"]).alias("l"))
        .field("due", Field::date().nullable())
        .id("id")
        .default_field("title")
        .predicate("pressing", "p:high has:due")
        .order("due", Order::by(["due"]))
        .build()?;

    let rows: Vec<Row> = [
        json!({ "id": 1, "title": "Fix login", "priority": "high", "labels": ["bug"], "due": "2026-11-01" }),
        json!({ "id": 2, "title": "Write guide", "priority": "low", "labels": ["docs"], "due": null }),
        json!({ "id": 3, "title": "Fix deploy", "priority": "normal", "labels": ["bug"], "due": "2026-10-20" }),
    ]
    .into_iter()
    .filter_map(|row| row.as_object().cloned())
    .collect();
    let backend = MemoryBackend::new(&tasks, rows);

    let query = parse(&tasks, "fix l:bug p>=normal order:due", &ParseOptions::default())?;
    let page = backend.search(&query, &PageRequest::first(20))?;
    let ids: Vec<i64> = page.rows.iter().filter_map(|row| row["id"].as_i64()).collect();
    assert_eq!(ids, [3, 1]);

    // A query that does not fit the collection is refused, with a position.
    let error = parse(&tasks, "fix title>5", &ParseOptions::default()).unwrap_err();
    assert_eq!(error.issues[0].code, "invalid_operator");
    assert_eq!(error.issues[0].span.map(|span| (span.start, span.end)), Some((4, 11)));
    Ok(())
}
```

`examples/server.rs` serves a collection over HTTP from SQLite:

```text
cargo run --example server
curl "http://localhost:8000/tasks?q=p>=high+l:bug+or+is:overdue&order=created"
curl "http://localhost:8000/tasks/syntax.md"
```

## The query syntax

| Write | Meaning |
|---|---|
| `word` | The default text field contains the word |
| `"two words"` | It contains the phrase |
| `!"the whole text"` | It is exactly this |
| `field:value` | The field matches, in the way natural to its kind |
| `field=value` `!=` `<` `>` `<=` `>=` | Comparisons, where the field's kind has them |
| `a b` | Both |
| `a or b` | Either. `or` binds more loosely than a space |
| `-a` | Not. Applies to a term or a group |
| `(a or b) c` | Grouping |
| `is:name` `not:name` | A named filter declared on the collection |
| `has:field` | The field has a value, or a set is not empty |
| `spent>estimate` | One field against another of the same kind |
| `title:/^fix/` | A regular expression, on text fields that allow it |
| `order:name` `direction:desc` | A declared sort order |
| `unique:name` `prefer:name` | One row per group, and which row to keep |
| `include:name` | Show rows the collection hides by default |

Field names, aliases and enum values ignore letter case. A null never satisfies a
comparison, so its negation always does.

### Field kinds

| Kind | `:` means | Other operators | Values |
|---|---|---|---|
| text | contains, ignoring case | `=` `!=` | a word, a phrase, or `/regex/` |
| keyword | equals | `=` `!=` | a value, optionally from a fixed list |
| number | equals | all | a number, or another number field |
| ordinal | equals | all, by rank | one of its ranked values |
| set | has every listed member | `=` exactly, `<=` within, `>=` has all, `<` `>` strict | members separated by commas, or a number for the size |
| date | falls within | all | `2026`, `2026-03`, `2026-03-15`, `now`, `today`, or another date field |

A date literal covers a range, so `created=2026` means "during 2026" and
`created>2026` means "after the end of 2026". All dates are UTC.

## Backends

Every backend returns the same rows in the same order for the same query. Each is a
cargo feature, on by default; turn off the ones you do not need.

| Backend | Feature | Sets stored as | Text comparison | Regular expressions |
|---|---|---|---|---|
| In memory | always | array | exact | linear-time |
| SQLite | `sqlite` | JSON text | exact | linear-time |
| PostgreSQL | `postgres` | `jsonb` or `text[]` | exact | the server's engine |
| MySQL, MariaDB | `mysql` | JSON | by collation, ignores case by default | the server's engine |
| DuckDB | `duckdb` | `VARCHAR[]` | exact | linear-time |
| SQL Server | `mssql` | JSON text | by collation, ignores case by default | 2025 and later only, opt-in |
| ClickHouse | `clickhouse` | `Array(String)` | exact | linear-time |
| BigQuery | `bigquery` | `ARRAY<STRING>` | exact | linear-time |
| MongoDB | `mongodb` | array | exact | the server's engine |
| Elasticsearch, OpenSearch | `elastic` | `keyword` array | exact | refused |

Date fields can be stored as the database's timestamp type, its date type, or ISO-8601
text, chosen per field. Whatever the storage and the session or column time zone,
dates are compared and returned as UTC.

Things worth knowing before using one in earnest:

- **Each SQL backend holds one connection behind a lock.** There is no pooling yet.
- **Every SQL search runs two statements**, the page and the count of all matches. On
  BigQuery both are billed. Elasticsearch and OpenSearch answer both in one request.
- **On Elasticsearch and OpenSearch, text still matches by substring**, not by analysed
  words, so results agree with every other backend. A rollup (`unique:`) takes two
  requests and is refused past a set number of groups, 10,000 by default.
- **Patterns are checked against Rust's `regex` syntax when a query is parsed**, so
  backreferences and lookaround are refused everywhere. Where the server's own engine
  does the matching, it may read a few constructs differently.
- **The `duckdb` feature compiles DuckDB from source**, which takes several minutes
  the first time.
- **The `mssql` and `mongodb` backends drive async clients on runtimes of their own.**
  Call them from a blocking context, not directly from an async task.
- **A MongoDB date keeps milliseconds only**, as does a plain `date` in Elasticsearch
  and OpenSearch. Rows closer together than that are told apart by id alone.
- **The `mysql` driver is built without TLS.** Turn on one of the `mysql` crate's TLS
  features before connecting to a remote server.

## Paging

A search returns a page and, if there is more, a `next` cursor. Passing it back as
`PageRequest::after` returns the rows that follow, however deep, and rows added or
removed in between do not shift later pages. A cursor only fits the order it was
issued for. `PageRequest::offset` jumps to a position by counting instead.

## Over HTTP

`http::respond` takes a request's path and query string and returns a status and a
JSON body. It is independent of any web framework.

```text
GET /tasks?q=<query>&order=<name>&dir=<asc|desc>&unique=<name>&prefer=<name>&include=<a,b>
```

A directive inside `q` overrides the same parameter. Follow `next_page` to walk a
result; `page=N` jumps by counting.

```json
{ "object": "list", "data": [], "has_more": false, "next_page": null, "total": 0, "warnings": [] }
```

```json
{
  "object": "error", "status": 400, "code": "bad_request", "type": "unknown_field",
  "details": "Unknown field \"nope\"",
  "issues": [{ "code": "unknown_field", "message": "Unknown field \"nope\"", "span": { "start": 6, "end": 12 } }]
}
```

Spans count characters, end exclusive. By default an invalid term rejects the request;
`HandlerOptions::lenient` drops it and reports it under `warnings` instead.

## A guide for the people writing queries

`syntax::describe` returns a collection's vocabulary as JSON: each field with its
aliases, operators, values and examples, and each named filter and order.
`syntax::guide` renders the same as Markdown. Both come from the declaration, so they
cannot drift from what the parser accepts, and a test parses every example in them.
Add plain-word descriptions with `Field::describe` and `CollectionBuilder::describe`.

## Tests

One table of 70 queries and their expected rows, in `tests/common/cases.rs`, is run
against every backend, along with paging, row decoding and time zone checks.

```text
cargo test
```

The in-memory, SQLite and DuckDB tests need nothing. The rest need servers, which
`scripts/test-databases.sh` starts as local containers; `.cargo/config.toml` holds the
matching connection settings (`DATABASE_URL` is PostgreSQL). A database is skipped when
its variable is empty:

```text
MYSQL_URL= MARIADB_URL= cargo test
```

BigQuery is billed by the query, so it only runs when asked for, with a token from the
signed-in `gcloud` CLI:

```text
BIGQUERY_PROJECT=my-project BIGQUERY_DATASET=indexer_test cargo test --test bigquery real
```

## License

Licensed under the [Apache License, Version 2.0](LICENSE.txt). Unless you say otherwise,
any contribution you submit for inclusion in this project is licensed under the same
terms, with no additional conditions.
