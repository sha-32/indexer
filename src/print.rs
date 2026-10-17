//! Canonical text for a query: full field names, explicit default field, minimal quoting.
//! Parsing the result against the same collection gives back an equal query.

use crate::ast::{Direction, Node, Query, Value};

#[derive(Clone, Copy, PartialEq, Eq)]
enum Parent {
    Top,
    And,
    Or,
    Not,
}

/// Prints a query, directives included.
pub fn print(query: &Query) -> String {
    let mut directives = Vec::new();
    if let Some(order) = &query.order {
        directives.push(format!("order:{order}"));
    }
    match query.direction {
        Direction::Auto => {}
        Direction::Asc => directives.push("direction:asc".to_string()),
        Direction::Desc => directives.push("direction:desc".to_string()),
    }
    if let Some(unique) = &query.unique {
        directives.push(format!("unique:{unique}"));
    }
    if let Some(prefer) = &query.prefer {
        directives.push(format!("prefer:{prefer}"));
    }
    directives.extend(query.include.iter().map(|name| format!("include:{name}")));

    let parent = if directives.is_empty() { Parent::Top } else { Parent::And };
    let filter = node(&query.filter, parent);
    let mut parts = Vec::new();
    if !filter.is_empty() {
        parts.push(filter);
    }
    parts.extend(directives);
    parts.join(" ")
}

/// Prints a filter on its own.
pub fn print_node(filter: &Node) -> String {
    node(filter, Parent::Top)
}

fn node(filter: &Node, parent: Parent) -> String {
    match filter {
        Node::All => String::new(),
        Node::And(children) => {
            let text = children.iter().map(|child| node(child, Parent::And)).collect::<Vec<_>>().join(" ");
            if matches!(parent, Parent::Top | Parent::And) { text } else { format!("({text})") }
        }
        Node::Or(children) => {
            let text = children.iter().map(|child| node(child, Parent::Or)).collect::<Vec<_>>().join(" or ");
            if parent == Parent::Top { text } else { format!("({text})") }
        }
        Node::Not(child) => format!("-{}", node(child, Parent::Not)),
        Node::Cmp { field, op, value } => format!("{field}{}{}", op.as_str(), print_value(value)),
        Node::Has(field) => format!("has:{field}"),
        Node::Is(name) => format!("is:{name}"),
    }
}

fn print_value(value: &Value) -> String {
    match value {
        Value::Text(text) | Value::Keyword(text) | Value::Ordinal(text) => quote(text),
        Value::Regex(source) => format!("/{}/", source.replace('/', "\\/")),
        Value::Number(number) => number.to_string(),
        Value::Count(count) => count.to_string(),
        Value::Set(members) => members.join(","),
        Value::Date { text, .. } => text.clone(),
        Value::Ref(field) => field.clone(),
    }
}

fn quote(text: &str) -> String {
    let plain = !text.is_empty()
        && !text.starts_with('/')
        && !text.chars().any(|ch| ch.is_whitespace() || "()\"".contains(ch));
    if plain {
        return text.to_string();
    }
    format!("\"{}\"", text.replace('\\', "\\\\").replace('"', "\\\""))
}
