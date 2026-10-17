mod common;

use common::{CASES, NOW_MS, query, tickets};
use indexer::{
    Collection, Direction, Directives, Field, Node, Op, ParseOptions, QueryError, RawValue, Span, Value,
    compare, parse, print, print_node,
};

fn options() -> ParseOptions {
    ParseOptions { now_ms: Some(NOW_MS), ..ParseOptions::default() }
}

fn failure(input: &str) -> QueryError {
    parse(&tickets(), input, &options()).expect_err(input)
}

fn cmp(field: &str, op: Op, value: Value) -> Node {
    Node::Cmp { field: field.to_string(), op, value }
}

#[test]
fn and_binds_tighter_than_or_and_parentheses_override_it() {
    let tickets = tickets();
    assert!(matches!(query(&tickets, "p>=high l:bug or l:docs").filter, Node::Or(_)));
    assert!(matches!(query(&tickets, "p>=high (l:bug or l:docs)").filter, Node::And(_)));
    assert_eq!(query(&tickets, "a:kim OR a:lee"), query(&tickets, "a:kim or a:lee"));
    assert_eq!(query(&tickets, "a:kim and p:high"), query(&tickets, "a:kim p:high"));
}

#[test]
fn bare_words_match_the_default_field_and_bang_makes_them_exact() {
    let tickets = tickets();
    assert_eq!(query(&tickets, "login").filter, cmp("title", Op::Colon, Value::Text("login".into())));
    assert_eq!(
        query(&tickets, "-!\"dark mode\"").filter,
        Node::not(cmp("title", Op::Eq, Value::Text("dark mode".into())))
    );
}

#[test]
fn names_aliases_and_enum_values_resolve_without_regard_to_case() {
    let tickets = tickets();
    assert_eq!(
        query(&tickets, "P>=HIGH L:Bug EST>5"),
        query(&tickets, "priority>=high labels:bug estimate>5")
    );
    assert_eq!(query(&tickets, "l:eng").filter, query(&tickets, "l:bug,infra").filter);
    assert_eq!(query(&tickets, "not:open").filter, query(&tickets, "-is:open").filter);
    assert_eq!(query(&tickets, "spent>EST").filter, cmp("spent", Op::Gt, Value::Ref("estimate".into())));
}

#[test]
fn date_literals_resolve_to_the_range_they_cover() {
    let tickets = tickets();
    // 15:00 on the day `now` is fixed to, to tell `now` from `today`.
    let options = ParseOptions { now_ms: Some(NOW_MS + 15 * 3_600_000), ..ParseOptions::default() };
    let range = |input: &str| match parse(&tickets, input, &options).expect(input).filter {
        Node::Cmp { value: Value::Date { start, end, .. }, .. } => (start, end),
        other => panic!("{input} resolved to {other:?}"),
    };
    let pair = |start: &str, end: &str| (start.to_string(), end.to_string());
    assert_eq!(range("created=2026"), pair("2026-01-01", "2027-01-01"));
    assert_eq!(range("created=2026-02"), pair("2026-02-01", "2026-03-01"));
    assert_eq!(range("created=2026-12"), pair("2026-12-01", "2027-01-01"));
    assert_eq!(range("created=2026-12-31"), pair("2026-12-31", "2027-01-01"));
    assert_eq!(range("created=2024-02-29"), pair("2024-02-29", "2024-03-01"));
    assert_eq!(range("created=today"), pair("2026-03-10", "2026-03-11"));
    assert_eq!(range("created<now"), pair("2026-03-10T15:00:00.000Z", "2026-03-10T15:00:00.000Z"));
    for impossible in ["created=2026-02-30", "created=2026-13", "created=2025-02-29", "created=26-01-01"] {
        assert_eq!(failure(impossible).issues[0].code, "invalid_value", "{impossible}");
    }
}

#[test]
fn directives_are_collected_and_the_last_one_wins() {
    let tickets = tickets();
    let all =
        query(&tickets, "l:bug order:created direction:asc unique:story prefer:newest include:archived");
    assert_eq!(all.order.as_deref(), Some("created"));
    assert_eq!(all.direction, Direction::Asc);
    assert_eq!(all.unique.as_deref(), Some("story"));
    assert_eq!(all.prefer.as_deref(), Some("newest"));
    assert_eq!(all.include, ["archived"]);
    assert_eq!(query(&tickets, "sort:title order:created").order.as_deref(), Some("created"));
    assert_eq!(query(&tickets, "order:created").filter, Node::All);

    // A directive given outside the query is a default that the query overrides.
    let defaults = Directives { order: Some("created".into()), ..Directives::default() };
    let options = ParseOptions { defaults, ..ParseOptions::default() };
    assert_eq!(parse(&tickets, "order:title", &options).unwrap().order.as_deref(), Some("title"));
    assert_eq!(parse(&tickets, "l:bug", &options).unwrap().order.as_deref(), Some("created"));
}

#[test]
fn invalid_terms_are_rejected_with_a_code_and_a_position() {
    let cases = [
        ("l:bug nope:1", "unknown_field", 6, 12),
        ("title>5", "invalid_operator", 0, 7),
        ("p:purple", "invalid_value", 0, 8),
        ("est>lots", "invalid_value", 0, 8),
        ("est>inf", "invalid_value", 0, 7),
        ("l:bug,nope", "invalid_value", 0, 10),
        ("is:sticky", "unknown_predicate", 0, 9),
        ("has:nope", "unknown_field", 0, 8),
        ("order:bogus", "unknown_order", 0, 11),
        ("direction:up", "invalid_direction", 0, 12),
        ("-order:created", "misplaced_directive", 1, 14),
        ("body:/x/", "regex_not_allowed", 0, 8),
        ("title:/(/", "invalid_regex", 0, 9),
        ("title=/x/", "invalid_operator", 0, 9),
        ("(l:bug", "syntax", 0, 1),
        ("l:bug)", "syntax", 5, 6),
        ("title:\"open", "syntax", 6, 11),
        ("l:bug or", "syntax", 8, 9),
        ("est>", "syntax", 0, 4),
        ("()", "syntax", 1, 2),
        // Positions count characters, not bytes.
        ("é:1 nope:1", "unknown_field", 4, 10),
    ];
    for (input, code, start, end) in cases {
        let issue = &failure(input).issues[0];
        assert_eq!((issue.code, issue.span), (code, Some(Span { start, end })), "{input}");
    }
    assert_eq!(failure("nope:1 p:purple").issues.len(), 2);
}

#[test]
fn patterns_a_linear_time_engine_cannot_run_are_refused() {
    // Backreferences and lookaround are what let a pattern take exponential time.
    for pattern in [r"title:/(a)\1/", "title:/(?=a)b/", "title:/(?<!a)b/"] {
        assert_eq!(failure(pattern).issues[0].code, "invalid_regex", "{pattern}");
    }
}

#[test]
fn lenient_mode_drops_invalid_terms_and_reports_them() {
    let tickets = tickets();
    let lenient = ParseOptions { lenient: true, ..ParseOptions::default() };
    let parsed = parse(&tickets, "l:bug nope:1 order:bogus", &lenient).unwrap();
    assert_eq!(parsed.filter, query(&tickets, "l:bug").filter);
    let codes: Vec<&str> = parsed.warnings.iter().map(|issue| issue.code).collect();
    assert_eq!(codes, ["unknown_field", "unknown_order"]);
    // Nothing valid is left to filter on, so matching everything would be a surprise.
    assert!(parse(&tickets, "nope:1", &lenient).is_err());
    // Syntax errors are never recoverable.
    assert!(parse(&tickets, "(l:bug", &lenient).is_err());
}

#[test]
fn printing_is_canonical_and_parses_back_to_the_same_query() {
    let tickets = tickets();
    for (input, _) in CASES {
        let first = query(&tickets, input);
        let printed = print(&first);
        assert_eq!(query(&tickets, &printed), first, "{input} -> {printed}");
        assert_eq!(print(&query(&tickets, &printed)), printed);
    }
    assert_eq!(
        print(&query(&tickets, "P>=High (L:bug OR a:kim) -\"dark mode\" sort:created dir:asc")),
        "priority>=high (labels:bug or assignee:kim) -title:\"dark mode\" order:created direction:asc"
    );
    assert_eq!(
        print(&query(&tickets, "a:kim or a:lee order:title")),
        "(assignee:kim or assignee:lee) order:title"
    );
    assert_eq!(print(&query(&tickets, "est<=2.50 est>5")), "estimate<=2.5 estimate>5");
}

#[test]
fn a_filter_built_in_code_is_the_tree_the_parser_produces() {
    let tickets = tickets();
    let term = |field: &str, op: Op, value: RawValue| compare(&tickets, field, op, &value).unwrap();
    let built = Node::and([
        term("priority", Op::Ge, RawValue::quoted("high")),
        term("l", Op::Ge, RawValue::quoted("bug,infra")),
        Node::not(term("assignee", Op::Eq, RawValue::quoted("kim"))),
        Node::or([
            term("estimate", Op::Gt, RawValue::word("5")),
            Node::Is("overdue".into()),
            term("spent", Op::Gt, RawValue::word("estimate")),
        ]),
        term("title", Op::Colon, RawValue::quoted("dark mode")),
        term("title", Op::Colon, RawValue::regex(r"^\d+%")),
        term("closed", Op::Gt, RawValue::word("created")),
        Node::Has("assignee".into()),
    ]);
    let parsed = query(
        &tickets,
        "p>=high l>=bug,infra -a=kim (est>5 or is:overdue or spent>estimate) title:\"dark mode\" \
         title:/^\\d+%/ closed>created has:assignee",
    );
    assert_eq!(built, parsed.filter);
    assert_eq!(print_node(&built), print(&parsed));

    let refused = compare(&tickets, "created", Op::Eq, &RawValue::quoted("yesterday")).unwrap_err();
    assert_eq!(refused.issues[0].code, "invalid_value");
    let refused = compare(&tickets, "title", Op::Gt, &RawValue::quoted("a")).unwrap_err();
    assert_eq!(refused.issues[0].code, "invalid_operator");
}

#[test]
fn an_inconsistent_collection_is_rejected_when_it_is_declared() {
    let base = || {
        Collection::builder("t")
            .field("id", Field::number())
            .field("title", Field::text())
            .id("id")
            .default_field("title")
    };
    let error = |builder: indexer::CollectionBuilder| builder.build().unwrap_err().to_string();
    assert!(base().build().is_ok());
    assert!(error(base().field("order", Field::number())).contains("reserved word"));
    assert!(error(base().field("name", Field::text().alias("Title"))).contains("more than once"));
    assert!(error(base().id("title2")).contains("unknown field"));
    assert!(error(base().default_field("id")).contains("must be a text field"));
    assert!(error(base().field("tags", Field::set(["a b"]))).contains("punctuation"));
    assert!(
        error(base().field("tags", Field::set(["a"])).order("x", indexer::Order::by(["tags"])))
            .contains("cannot use set field")
    );
    assert_eq!(
        error(base().predicate("odd", "nope:1")),
        "Collection \"t\": predicate \"odd\" \"nope:1\" is invalid: Unknown field \"nope\""
    );
    assert!(error(base().exclusion("odd", "order:x")).contains("is invalid"));
    assert!(
        error(base().predicate("later", "is:other").predicate("other", "id=1")).contains("Unknown predicate")
    );
}
