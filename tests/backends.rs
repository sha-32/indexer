mod common;

use common::{CASES, ids, query, rows, sqlite_database, tickets, walk};
use indexer::backend::postgres::{PostgresDialect, PostgresOptions};
use indexer::backend::sql::{Param, compile_sql};
use indexer::backend::sqlite::{SqliteDialect, sqlite_backend};
use indexer::http::{HandlerOptions, respond};
use indexer::{Backend, Error, MemoryBackend, PageRequest};
use serde_json::{Value as Json, json};

fn backends() -> Vec<(&'static str, Box<dyn Backend>)> {
    let tickets = tickets();
    vec![
        ("memory", Box::new(MemoryBackend::new(&tickets, rows()))),
        ("sqlite", Box::new(sqlite_backend(&tickets, sqlite_database()).unwrap())),
    ]
}

#[test]
fn every_case_returns_the_expected_rows_in_order() {
    let tickets = tickets();
    for (name, backend) in backends() {
        for (input, expected) in CASES {
            let page = backend.search(&query(&tickets, input), &PageRequest::first(100)).unwrap();
            assert_eq!(ids(&page.rows), *expected, "{name}: {input}");
            assert_eq!(page.total, expected.len() as u64, "{name}: {input}");
            assert_eq!((page.has_more, &page.next), (false, &None), "{name}: {input}");
        }
    }
}

#[test]
fn following_cursors_visits_every_row_once_in_order() {
    let tickets = tickets();
    for (name, backend) in backends() {
        for (input, expected) in CASES {
            for limit in [1, 2, 4] {
                let walked = walk(backend.as_ref(), &query(&tickets, input), limit);
                assert_eq!(walked, *expected, "{name}: {input} (limit {limit})");
            }
        }
    }
}

#[test]
fn a_cursor_only_fits_the_order_it_was_issued_for() {
    use base64::Engine;
    let tickets = tickets();
    for (name, backend) in backends() {
        let first = backend.search(&query(&tickets, "order:created"), &PageRequest::first(2)).unwrap();
        let cursor = first.next.expect("a second page");
        let forged = |parts: Json| base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(parts.to_string());
        let refused = [
            ("order:title", cursor.clone()),
            ("order:created direction:asc", cursor.clone()),
            ("order:created", "not-a-cursor".to_string()),
            // The right order, but a number where a date belongs, then too few values.
            ("order:created", forged(json!(["created desc last,id asc last", 5, 1]))),
            ("order:created", forged(json!(["created desc last,id asc last", "2026-01-01"]))),
            // A null for a field that cannot be null.
            ("order:created", forged(json!(["created desc last,id asc last", null, 1]))),
        ];
        for (order, token) in refused {
            match backend.search(&query(&tickets, order), &PageRequest::after(2, token.as_str())) {
                Err(Error::Query(error)) => assert_eq!(error.issues[0].code, "invalid_cursor", "{name}"),
                other => panic!("{name}: {order} with {token} gave {other:?}"),
            }
        }
    }
}

#[test]
fn pages_are_slices_of_one_stable_order() {
    let tickets = tickets();
    for (name, backend) in backends() {
        let by_priority = query(&tickets, "order:priority");
        let page = backend.search(&by_priority, &PageRequest::first(3).offset(2)).unwrap();
        assert_eq!(ids(&page.rows), [4, 2, 9], "{name}");
        assert_eq!((page.total, page.has_more), (9, true), "{name}");
        let last = backend.search(&by_priority, &PageRequest::first(3).offset(6)).unwrap();
        assert_eq!((ids(&last.rows), last.has_more, last.next), (vec![5, 6, 3], false, None), "{name}");
        // An offset counts on from a cursor, so the two can be combined.
        let skipped =
            backend.search(&by_priority, &PageRequest::after(2, page.next.unwrap()).offset(1)).unwrap();
        assert_eq!(ids(&skipped.rows), [5, 6], "{name}");
    }
}

#[test]
fn sqlite_returns_the_same_rows_as_memory() {
    let tickets = tickets();
    let everything = query(&tickets, "include:archived");
    let found: Vec<_> = backends()
        .into_iter()
        .map(|(_, backend)| backend.search(&everything, &PageRequest::first(100)).unwrap().rows)
        .collect();
    assert_eq!(found[0], rows());
    assert_eq!(found[1], found[0]);
}

#[test]
fn rows_added_or_removed_between_pages_do_not_shift_the_pages_after() {
    let tickets = tickets();
    let backend = sqlite_backend(&tickets, sqlite_database()).unwrap();
    let by_created = query(&tickets, "order:created");
    let first = backend.search(&by_created, &PageRequest::first(3)).unwrap();
    assert_eq!(ids(&first.rows), [9, 8, 6]);

    // A newer ticket arrives and one already seen is deleted. By offset, the next page
    // would start one row late or early; by cursor it starts right after ticket 6.
    backend.runner().with(|connection| {
        connection
            .execute_batch(
                "INSERT INTO tickets VALUES
                   (11, 'New', NULL, 'open', 'low', '[]', 1, 1, '2026-05-01', NULL, NULL, 'x', 0);
                 DELETE FROM tickets WHERE id = 8;",
            )
            .unwrap();
    });
    let second = backend.search(&by_created, &PageRequest::after(3, first.next.unwrap())).unwrap();
    assert_eq!(ids(&second.rows), [5, 4, 3]);
}

#[test]
fn a_pattern_that_would_stall_a_backtracking_engine_returns_at_once() {
    let tickets = tickets();
    let hostile = "title:/^(a+)+$/";
    let mut long = rows();
    long[0].insert("title".into(), json!(format!("{}!", "a".repeat(5_000))));
    let memory = MemoryBackend::new(&tickets, long);
    let started = std::time::Instant::now();
    assert_eq!(memory.search(&query(&tickets, hostile), &PageRequest::first(10)).unwrap().total, 0);

    let sqlite = sqlite_backend(&tickets, sqlite_database()).unwrap();
    sqlite.runner().with(|connection| {
        connection
            .execute("UPDATE tickets SET title = ? WHERE id = 1", [format!("{}!", "a".repeat(5_000))])
            .unwrap();
    });
    assert_eq!(sqlite.search(&query(&tickets, hostile), &PageRequest::first(10)).unwrap().total, 0);
    assert!(started.elapsed().as_secs() < 5, "took {:?}", started.elapsed());
}

#[test]
fn values_are_sent_as_parameters_never_as_sql_text() {
    let tickets = tickets();
    let hostile = query(&tickets, "title:\"x' OR 1=1 --\" a:\"kim'; DROP TABLE tickets\"");
    let compiled =
        compile_sql(&tickets, &hostile, &PageRequest::first(100), &SqliteDialect, "tickets").unwrap();
    assert!(!compiled.page.sql.contains("DROP") && !compiled.page.sql.contains("1=1"));
    assert_eq!(
        compiled.page.params[..2],
        [Param::Text("%x' or 1=1 --%".into()), Param::Text("kim'; DROP TABLE tickets".into())]
    );
}

#[test]
fn postgres_numbers_its_placeholders_in_the_order_of_the_parameters() {
    let tickets = tickets();
    let dialect = PostgresDialect::new(PostgresOptions::default());
    let parsed = query(&tickets, "p>=high closed>=2026 l:bug est<2.5 -title:/x/ unique:story order:priority");
    let page = PageRequest::first(5).offset(10);
    let compiled = compile_sql(&tickets, &parsed, &page, &dialect, "tickets").unwrap();

    for statement in [&compiled.page, &compiled.count] {
        let used: Vec<usize> = statement
            .sql
            .split('$')
            .skip(1)
            .map(|rest| rest.chars().take_while(char::is_ascii_digit).collect::<String>().parse().unwrap())
            .collect();
        assert_eq!(used, (1..=statement.params.len()).collect::<Vec<_>>());
    }
    assert_eq!(compiled.page.params[compiled.page.params.len() - 2..], [Param::Int(6), Param::Int(10)]);
    assert_eq!(
        compiled.count.sql,
        "SELECT COUNT(DISTINCT t.\"story\") AS \"n\" FROM \"tickets\" AS t WHERE (t.\"priority\" IN ($1, $2) AND \
         (t.\"closed_at\" IS NOT NULL AND t.\"closed_at\" >= $3::timestamptz) AND t.\"labels\" @> $4::jsonb AND \
         (t.\"estimate\" IS NOT NULL AND t.\"estimate\" < $5::float8) AND \
         NOT (t.\"title\" ~* $6) AND NOT (t.\"archived\" = $7))"
    );
    let text = |value: &str| Param::Text(value.to_string());
    assert_eq!(
        compiled.count.params,
        [
            text("high"),
            text("urgent"),
            text("2026-01-01T00:00:00Z"),
            text("[\"bug\"]"),
            Param::Real(2.5),
            text("x"),
            Param::Int(1)
        ]
    );
}

fn get(target: &str) -> indexer::http::Response {
    let tickets = tickets();
    let backend = sqlite_backend(&tickets, sqlite_database()).unwrap();
    let options = HandlerOptions { page_size: 4, ..HandlerOptions::default() };
    respond(&tickets, &backend, target, &options)
}

fn data_ids(body: &Json) -> Vec<i64> {
    body["data"].as_array().unwrap().iter().map(|row| row["id"].as_i64().unwrap()).collect()
}

#[test]
fn http_a_search_returns_a_list_and_a_link_to_the_next_page() {
    let first = get("/tickets?q=is%3Aopen&order=created");
    assert_eq!(first.status, 200);
    assert_eq!(first.body["object"], "list");
    assert_eq!(data_ids(&first.body), [9, 8, 6, 5]);
    assert_eq!((&first.body["has_more"], &first.body["total"]), (&json!(true), &json!(7)));
    assert_eq!(first.body["warnings"], json!([]));

    let next = first.body["next_page"].as_str().unwrap();
    assert!(next.starts_with("/tickets?q=is%3Aopen&order=created&cursor="), "{next}");
    let second = get(next);
    assert_eq!(data_ids(&second.body), [4, 2, 1]);
    assert_eq!((&second.body["has_more"], &second.body["next_page"]), (&json!(false), &Json::Null));
}

#[test]
fn http_a_page_reached_by_number_links_onward_by_cursor() {
    let second = get("/?order=created&page=2");
    assert_eq!(data_ids(&second.body), [4, 3, 2, 1]);
    let next = second.body["next_page"].as_str().unwrap();
    assert!(next.contains("cursor=") && !next.contains("page="), "{next}");
    let third = get(next);
    assert_eq!((data_ids(&third.body), &third.body["next_page"]), (vec![7], &Json::Null));
}

#[test]
fn http_a_directive_in_the_query_overrides_the_parameter() {
    assert_eq!(data_ids(&get("/?q=p%3Aurgent+order%3Atitle&order=created").body), [7, 1]);
    assert_eq!(data_ids(&get("/?q=p%3Aurgent&order=title&dir=desc").body), [1, 7]);
    assert_eq!(data_ids(&get("/?q=login&include=archived").body), [1, 2, 10]);
}

#[test]
fn http_no_matches_is_an_empty_list_not_an_error() {
    let response = get("/?q=nothingmatchesthis");
    assert_eq!(response.status, 200);
    assert_eq!(response.body["data"], json!([]));
    assert_eq!((&response.body["total"], &response.body["has_more"]), (&json!(0), &json!(false)));
}

#[test]
fn http_a_bad_query_is_a_400_that_says_where_the_problem_is() {
    let response = get("/?q=l%3Abug+nope%3A1");
    assert_eq!(response.status, 400);
    assert_eq!(
        response.body,
        json!({
            "object": "error",
            "status": 400,
            "code": "bad_request",
            "type": "unknown_field",
            "details": "Unknown field \"nope\"",
            "issues": [{
                "code": "unknown_field",
                "message": "Unknown field \"nope\"",
                "span": { "start": 6, "end": 12 },
            }],
        })
    );
    for (target, kind) in [
        ("/?order=bogus", "unknown_order"),
        ("/?page=0", "invalid_page"),
        ("/?page=two", "invalid_page"),
        ("/?cursor=nonsense", "invalid_cursor"),
        ("/?cursor=nonsense&page=2", "invalid_page"),
    ] {
        let response = get(target);
        assert_eq!((response.status, &response.body["type"]), (400, &json!(kind)), "{target}");
    }
    let long = get(&format!("/?q={}", "a".repeat(1001)));
    assert_eq!((long.status, &long.body["type"]), (400, &json!("query_too_long")));
}

#[test]
fn http_lenient_mode_answers_with_warnings_instead() {
    let tickets = tickets();
    let backend = MemoryBackend::new(&tickets, rows());
    let options =
        HandlerOptions { lenient: true, base_url: "https://api.test".into(), ..HandlerOptions::default() };
    let response = respond(&tickets, &backend, "/?q=l%3Abug+nope%3A1", &options);
    assert_eq!(response.status, 200);
    assert_eq!(data_ids(&response.body), [1, 5, 7]);
    assert_eq!(response.body["warnings"][0]["code"], "unknown_field");

    let options = HandlerOptions { page_size: 2, ..options };
    let paged = respond(&tickets, &backend, "/t?q=l%3Abug", &options);
    assert!(paged.body["next_page"].as_str().unwrap().starts_with("https://api.test/t?q=l%3Abug&cursor="));
}
