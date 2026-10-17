//! A collection's query syntax, described for the people and programs that write queries.
//!
//! [`describe`] gives the vocabulary as data: every field with its aliases, operators,
//! values and worked examples, and every predicate, order and other named term.
//! [`guide`] renders the same thing as a Markdown reference.
//!
//! Both are made from the collection's declaration, so they cannot drift from what the
//! parser accepts, and every example in them is a query that parses.

use serde_json::{Value as Json, json};

use crate::collection::Collection;
use crate::print::print_node;
use crate::schema::{Field, FieldKind, SetColon};

fn example(query: impl Into<String>, meaning: impl Into<String>) -> Json {
    json!({ "query": query.into(), "meaning": meaning.into() })
}

/// What each operator means on a field, in the order a reader should meet them.
fn operators(field: &Field) -> Vec<(&'static str, String)> {
    match field.kind {
        FieldKind::Text => vec![
            (":", "contains the text, ignoring case".into()),
            ("=", "is exactly the text, ignoring case".into()),
            ("!=", "is not exactly the text".into()),
        ],
        FieldKind::Keyword => {
            vec![
                (":", "is the value".into()),
                ("=", "is the value".into()),
                ("!=", "is not the value".into()),
            ]
        }
        FieldKind::Number => vec![
            (":", "equals the number".into()),
            ("=", "equals the number".into()),
            ("!=", "does not equal the number".into()),
            (">", "is greater than the number".into()),
            (">=", "is the number or greater".into()),
            ("<", "is less than the number".into()),
            ("<=", "is the number or less".into()),
        ],
        FieldKind::Ordinal => vec![
            (":", "is the value".into()),
            ("=", "is the value".into()),
            ("!=", "is not the value".into()),
            (">", "ranks above the value".into()),
            (">=", "is the value or ranks above it".into()),
            ("<", "ranks below the value".into()),
            ("<=", "is the value or ranks below it".into()),
        ],
        FieldKind::Set => {
            let colon = match field.colon {
                SetColon::HasAll => "has every listed member",
                SetColon::Within => "has no member outside those listed",
                SetColon::Exactly => "has exactly the listed members",
            };
            vec![
                (":", colon.into()),
                ("=", "has exactly the listed members".into()),
                ("!=", "does not have exactly the listed members".into()),
                (">=", "has every listed member".into()),
                (">", "has every listed member and at least one more".into()),
                ("<=", "has no member outside those listed".into()),
                ("<", "has only some of the listed members and no others".into()),
            ]
        }
        FieldKind::Date => vec![
            (":", "falls within the date".into()),
            ("=", "falls within the date".into()),
            ("!=", "falls outside the date".into()),
            (">", "is after the end of the date".into()),
            (">=", "is on or after the start of the date".into()),
            ("<", "is before the start of the date".into()),
            ("<=", "is on or before the end of the date".into()),
        ],
    }
}

/// Worked examples for one field. Each is a query the collection accepts.
fn examples(collection: &Collection, name: &str, field: &Field) -> Vec<Json> {
    let mut found = Vec::new();
    // Another field of the same kind, to show a field-to-field comparison. Comparing
    // anything with the id would be a strange thing to teach.
    let other = collection
        .fields()
        .filter(|_| name != collection.id())
        .find(|(other, declared)| *other != name && *other != collection.id() && declared.kind == field.kind)
        .map(|(other, _)| other);

    match field.kind {
        FieldKind::Text => {
            if collection.default_field() == name {
                found.push(example("word", format!("{name} contains \"word\"")));
                found.push(example("\"two words\"", format!("{name} contains the phrase \"two words\"")));
                found.push(example("!\"the whole text\"", format!("{name} is exactly \"the whole text\"")));
            }
            found.push(example(format!("{name}:word"), format!("{name} contains \"word\"")));
            found.push(example(
                format!("{name}=\"the whole text\""),
                format!("{name} is exactly \"the whole text\""),
            ));
            if field.regex {
                found.push(example(
                    format!("{name}:/^start/"),
                    format!("{name} matches the regular expression ^start"),
                ));
            }
        }
        FieldKind::Keyword => {
            let value = field.values.first().map_or("value", String::as_str);
            found.push(example(format!("{name}:{value}"), format!("{name} is {value}")));
            found.push(example(format!("{name}!={value}"), format!("{name} is not {value}")));
        }
        FieldKind::Number => {
            found.push(example(format!("{name}=5"), format!("{name} is 5")));
            found.push(example(format!("{name}>=5"), format!("{name} is 5 or more")));
            found.push(example(format!("{name}<2.5"), format!("{name} is less than 2.5")));
            if let Some(other) = other {
                found.push(example(format!("{name}>{other}"), format!("{name} is greater than {other}")));
            }
        }
        FieldKind::Ordinal => {
            let middle = &field.values[field.values.len() / 2];
            let lowest = &field.values[0];
            found.push(example(format!("{name}:{lowest}"), format!("{name} is {lowest}")));
            found.push(example(format!("{name}>={middle}"), format!("{name} is {middle} or above")));
            found.push(example(format!("{name}<{middle}"), format!("{name} is below {middle}")));
        }
        FieldKind::Set => {
            let first = &field.values[0];
            let colon = |members: &str| match field.colon {
                SetColon::HasAll => format!("{name} has {members}"),
                SetColon::Within => format!("{name} has nothing but {members}"),
                SetColon::Exactly => format!("{name} has exactly {members}"),
            };
            found.push(example(format!("{name}:{first}"), colon(first)));
            if let Some(second) = field.values.get(1) {
                let both = format!("{first} and {second}");
                found.push(example(format!("{name}:{first},{second}"), colon(&both)));
                found.push(example(
                    format!("{name}<={first},{second}"),
                    format!("{name} has nothing other than {first} or {second}"),
                ));
            }
            found.push(example(format!("{name}={first}"), format!("{name} has {first} and nothing else")));
            if let Some((nickname, members)) = field.nicknames.first() {
                found.push(example(format!("{name}:{nickname}"), colon(&members.join(" and "))));
            }
            found.push(example(format!("{name}>=2"), format!("{name} has two or more members")));
            found.push(example(format!("{name}=0"), format!("{name} is empty")));
        }
        FieldKind::Date => {
            found.push(example(format!("{name}=2026"), format!("{name} is in 2026")));
            found.push(example(format!("{name}>=2026-03"), format!("{name} is in March 2026 or later")));
            found.push(example(format!("{name}<2026-03-15"), format!("{name} is before 15 March 2026")));
            found.push(example(format!("{name}>=today"), format!("{name} is today or later")));
            found.push(example(format!("{name}<now"), format!("{name} is in the past")));
            if let Some(other) = other {
                found.push(example(format!("{name}>{other}"), format!("{name} is after {other}")));
            }
        }
    }
    if field.nullable || field.kind == FieldKind::Set {
        let (has, lacks) = if field.kind == FieldKind::Set {
            ("is not empty", "is empty")
        } else {
            ("has a value", "has no value")
        };
        found.push(example(format!("has:{name}"), format!("{name} {has}")));
        found.push(example(format!("-has:{name}"), format!("{name} {lacks}")));
    }
    found
}

/// What a value of the field looks like, for a reader about to type one.
fn value_hint(field: &Field) -> &'static str {
    match field.kind {
        FieldKind::Text => "A word, or a \"quoted phrase\" if it has spaces or punctuation.",
        FieldKind::Keyword if field.values.is_empty() => "A value, matched exactly.",
        FieldKind::Keyword | FieldKind::Ordinal => "One of the listed values, in any letter case.",
        FieldKind::Number => "A number, or the name of another number field.",
        FieldKind::Set => "One or more members separated by commas, or a number to compare the size.",
        FieldKind::Date => {
            "A year (2026), a month (2026-03), a day (2026-03-15), now, today, or the name of \
             another date field. Dates are UTC."
        }
    }
}

/// How terms combine, with examples built from the collection's own default field.
fn basics(collection: &Collection) -> Json {
    let field = collection.default_field();
    json!([
        {
            "syntax": "word",
            "meaning": format!("A bare word searches {field}."),
            "examples": [example("word", format!("{field} contains \"word\""))],
        },
        {
            "syntax": "\"a phrase\"",
            "meaning": "Quotes keep spaces and punctuation together. Write \\\" for a quote inside.",
            "examples": [example("\"two words\"", format!("{field} contains the phrase \"two words\""))],
        },
        {
            "syntax": "!word",
            "meaning": format!("An exclamation mark matches the whole of {field}, not a part of it."),
            "examples": [example("!\"the whole text\"", format!("{field} is exactly \"the whole text\""))],
        },
        {
            "syntax": "field:value",
            "meaning": "A field name, an operator and a value, with no spaces between them. Field \
                        names and values ignore letter case.",
            "examples": [example(format!("{field}:word"), format!("{field} contains \"word\""))],
        },
        {
            "syntax": "a b",
            "meaning": "Terms separated by spaces must all match.",
            "examples": [example("first second", format!("{field} contains both \"first\" and \"second\""))],
        },
        {
            "syntax": "a or b",
            "meaning": "The word or between terms matches either. It binds more loosely than a space.",
            "examples": [example("first or second", format!("{field} contains \"first\" or \"second\""))],
        },
        {
            "syntax": "-a",
            "meaning": "A hyphen excludes what follows it: a term or a group.",
            "examples": [example("-word", format!("{field} does not contain \"word\""))],
        },
        {
            "syntax": "(a or b) c",
            "meaning": "Parentheses group terms.",
            "examples": [example(
                "(first or second) -third",
                format!("{field} contains \"first\" or \"second\", and not \"third\""),
            )],
        },
    ])
}

/// The collection's query vocabulary, as data.
pub fn describe(collection: &Collection) -> Json {
    let fields: Vec<Json> = collection
        .fields()
        .map(|(name, field)| {
            let operators: Vec<Json> = operators(field)
                .into_iter()
                .map(|(op, meaning)| json!({ "operator": op, "meaning": meaning }))
                .collect();
            let nicknames: serde_json::Map<String, Json> = field
                .nicknames
                .iter()
                .map(|(nickname, members)| (nickname.clone(), json!(members)))
                .collect();
            json!({
                "name": name,
                "aliases": field.aliases,
                "kind": field.kind.name(),
                "description": field.description,
                "nullable": field.nullable,
                "default": collection.default_field() == name,
                "operators": operators,
                "value": value_hint(field),
                "values": field.values,
                "nicknames": nicknames,
                "regex": field.regex,
                "examples": examples(collection, name, field),
            })
        })
        .collect();

    let described = |kind: &str, name: &str| collection.description(&format!("{kind}:{name}"));
    let predicates: Vec<Json> = collection
        .predicates()
        .map(|(name, filter)| {
            json!({
                "name": name,
                "description": described("is", name),
                "means": print_node(filter),
                "examples": [
                    example(format!("is:{name}"), format!("rows that are {name}")),
                    example(format!("not:{name}"), format!("rows that are not {name}")),
                ],
            })
        })
        .collect();
    let orders: Vec<Json> = collection
        .orders()
        .map(|(name, order)| {
            let (natural, reverse) = if order.descending { ("desc", "asc") } else { ("asc", "desc") };
            json!({
                "name": name,
                "description": described("order", name),
                "fields": order.by,
                "natural_direction": natural,
                "nulls": if order.nulls_first { "first" } else { "last" },
                "default": collection.default_order() == Some(name),
                "examples": [
                    example(format!("order:{name}"), format!("sort by {}, {natural}ending", order.by.join(" then "))),
                    example(
                        format!("order:{name} direction:{reverse}"),
                        format!("sort by {}, {reverse}ending", order.by.join(" then ")),
                    ),
                ],
            })
        })
        .collect();
    let rollups: Vec<Json> = collection
        .rollups()
        .map(|(name, by)| {
            json!({
                "name": name,
                "description": described("unique", name),
                "by": by,
                "examples": [example(format!("unique:{name}"), format!("one row for each {by}"))],
            })
        })
        .collect();
    let first_rollup = collection.rollups().next().map(|(name, _)| name);
    let preferences: Vec<Json> = collection
        .preferences()
        .map(|(name, preference)| {
            let which = if preference.descending { "highest" } else { "lowest" };
            let examples: Vec<Json> = first_rollup
                .iter()
                .map(|rollup| {
                    example(
                        format!("unique:{rollup} prefer:{name}"),
                        format!("one row for each {rollup}: the one with the {which} {}", preference.by),
                    )
                })
                .collect();
            json!({
                "name": name,
                "description": described("prefer", name),
                "by": preference.by,
                "keeps": which,
                "examples": examples,
            })
        })
        .collect();
    let exclusions: Vec<Json> = collection
        .exclusions()
        .map(|(name, filter)| {
            json!({
                "name": name,
                "description": described("include", name),
                "hides": print_node(filter),
                "examples": [example(format!("include:{name}"), format!("also show rows hidden as {name}"))],
            })
        })
        .collect();

    json!({
        "object": "syntax",
        "collection": collection.name(),
        "default_field": collection.default_field(),
        "basics": basics(collection),
        "fields": fields,
        "predicates": predicates,
        "orders": orders,
        "rollups": rollups,
        "preferences": preferences,
        "exclusions": exclusions,
    })
}

/// Collects every example query in a description, for checking that each one parses.
pub fn example_queries(description: &Json) -> Vec<String> {
    let mut queries = Vec::new();
    let mut pending = vec![description];
    while let Some(value) = pending.pop() {
        match value {
            Json::Array(items) => pending.extend(items),
            Json::Object(object) => {
                if let (Some(Json::String(query)), Some(_)) = (object.get("query"), object.get("meaning")) {
                    queries.push(query.clone());
                }
                pending.extend(object.values());
            }
            _ => {}
        }
    }
    queries
}

fn code(text: &str) -> String {
    // A backtick inside would end the code span early, so widen the fence around it.
    if text.contains('`') { format!("`` {text} ``") } else { format!("`{text}`") }
}

fn text_of<'a>(value: &'a Json, key: &str) -> &'a str {
    value[key].as_str().unwrap_or_default()
}

fn list_of<'a>(value: &'a Json, key: &str) -> impl Iterator<Item = &'a Json> {
    value[key].as_array().into_iter().flatten()
}

fn example_table(out: &mut String, examples: impl Iterator<Item = Json>) {
    let rows: Vec<Json> = examples.collect();
    if rows.is_empty() {
        return;
    }
    out.push_str("| Query | Finds |\n|---|---|\n");
    for row in rows {
        let query = text_of(&row, "query").replace('|', "\\|");
        out.push_str(&format!("| {} | {} |\n", code(&query), text_of(&row, "meaning")));
    }
    out.push('\n');
}

/// The collection's query syntax as a Markdown reference.
pub fn guide(collection: &Collection) -> String {
    let description = describe(collection);
    let mut out = format!("# Searching {}\n\n", collection.name());

    out.push_str("## The basics\n\n| Write | Meaning |\n|---|---|\n");
    for rule in list_of(&description, "basics") {
        out.push_str(&format!("| {} | {} |\n", code(text_of(rule, "syntax")), text_of(rule, "meaning")));
    }
    out.push('\n');
    example_table(
        &mut out,
        list_of(&description, "basics").flat_map(|rule| list_of(rule, "examples")).cloned(),
    );

    out.push_str("## Fields\n\n");
    for field in list_of(&description, "fields") {
        let name = text_of(field, "name");
        out.push_str(&format!("### {name}\n\n"));
        if let Some(text) = field["description"].as_str() {
            out.push_str(&format!("{text}\n\n"));
        }
        let aliases: Vec<String> = list_of(field, "aliases").filter_map(Json::as_str).map(code).collect();
        if !aliases.is_empty() {
            out.push_str(&format!("- **Also written:** {}\n", aliases.join(", ")));
        }
        out.push_str(&format!("- **Kind:** {}\n", text_of(field, "kind")));
        out.push_str(&format!("- **Value:** {}\n", text_of(field, "value")));
        let values: Vec<String> = list_of(field, "values").filter_map(Json::as_str).map(code).collect();
        if !values.is_empty() {
            let ranked = if text_of(field, "kind") == "ordinal" { ", lowest first" } else { "" };
            out.push_str(&format!("- **Values{ranked}:** {}\n", values.join(", ")));
        }
        for (nickname, members) in field["nicknames"].as_object().into_iter().flatten() {
            let members: Vec<String> =
                members.as_array().into_iter().flatten().filter_map(Json::as_str).map(code).collect();
            out.push_str(&format!("- **{} means:** {}\n", code(nickname), members.join(" and ")));
        }
        if field["regex"] == true {
            out.push_str("- **Regular expressions:** accepted between slashes, ignoring case.\n");
        }
        if field["nullable"] == true {
            out.push_str("- **May be empty:** a comparison never matches an empty value, and its negation always does.\n");
        }
        out.push_str("\n| Operator | Meaning |\n|---|---|\n");
        for operator in list_of(field, "operators") {
            out.push_str(&format!(
                "| {} | {} |\n",
                code(text_of(operator, "operator")),
                text_of(operator, "meaning")
            ));
        }
        out.push('\n');
        example_table(&mut out, list_of(field, "examples").cloned());
    }

    let section = |out: &mut String,
                   key: &str,
                   title: &str,
                   intro: &str,
                   detail: &dyn Fn(&Json) -> String| {
        if list_of(&description, key).next().is_none() {
            return;
        }
        out.push_str(&format!("## {title}\n\n{intro}\n\n"));
        for item in list_of(&description, key) {
            let text = item["description"].as_str().map(|text| format!(" {text}")).unwrap_or_default();
            out.push_str(&format!("- **{}**:{text} {}\n", text_of(item, "name"), detail(item)));
        }
        out.push('\n');
        example_table(out, list_of(&description, key).flat_map(|item| list_of(item, "examples")).cloned());
    };

    section(
        &mut out,
        "predicates",
        "Named filters",
        "Write `is:name` for rows that match one, and `not:name` for rows that do not.",
        &|item| format!("Short for {}.", code(text_of(item, "means"))),
    );
    section(
        &mut out,
        "orders",
        "Sorting",
        "Write `order:name` to sort, and `direction:asc` or `direction:desc` to choose the direction.",
        &|item| {
            let fields: Vec<&str> = list_of(item, "fields").filter_map(Json::as_str).collect();
            let default = if item["default"] == true { " Used when no order is given." } else { "" };
            format!(
                "By {}, {}ending unless told otherwise, empty values {}.{default}",
                fields.join(" then "),
                text_of(item, "natural_direction"),
                text_of(item, "nulls"),
            )
        },
    );
    section(
        &mut out,
        "rollups",
        "One row per group",
        "Write `unique:name` to keep a single row for each group.",
        &|item| format!("One row for each {}.", text_of(item, "by")),
    );
    section(
        &mut out,
        "preferences",
        "Choosing the row a group keeps",
        "Write `prefer:name` alongside `unique:` to choose which row stands for its group.",
        &|item| format!("Keeps the row with the {} {}.", text_of(item, "keeps"), text_of(item, "by")),
    );
    section(
        &mut out,
        "exclusions",
        "Hidden rows",
        "Some rows are left out unless asked for. Write `include:name` to show them.",
        &|item| format!("Hides rows matching {}.", code(text_of(item, "hides"))),
    );
    out
}
