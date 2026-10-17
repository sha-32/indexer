//! Syntax tree to resolved query: names, operators and values checked against a collection.

use regex::RegexBuilder;

use crate::ast::{Direction, Issue, Node, Query, QueryError, Span, Value};
use crate::collection::Collection;
use crate::parser::{Cst, RawKind, RawValue, parse_cst};
use crate::schema::{FieldKind, Op};
use crate::time;

/// Result-shaping directives from outside the query string, as untrusted text.
#[derive(Clone, Debug, Default)]
pub struct Directives {
    pub order: Option<String>,
    pub direction: Option<String>,
    pub unique: Option<String>,
    pub prefer: Option<String>,
    pub include: Vec<String>,
}

#[derive(Clone, Debug, Default)]
pub struct ParseOptions {
    /// Drop terms that fail to resolve and report them as warnings, instead of failing.
    /// A query whose every filter term was dropped still fails.
    pub lenient: bool,
    /// The instant `now` and `today` refer to, in milliseconds since the Unix epoch.
    /// Defaults to the current time.
    pub now_ms: Option<i64>,
    /// Directives from outside the query string. The same directive in the query wins.
    pub defaults: Directives,
}

/// Why one term was refused. It becomes an [`Issue`] once it has a position.
pub(crate) struct Rejection {
    code: &'static str,
    message: String,
}

fn reject<T>(code: &'static str, message: impl Into<String>) -> Result<T, Rejection> {
    Err(Rejection { code, message: message.into() })
}

const DIRECTIVES: [&str; 7] = ["order", "sort", "direction", "dir", "unique", "prefer", "include"];

/// Parses a query and resolves it against a collection.
pub fn parse(collection: &Collection, input: &str, options: &ParseOptions) -> Result<Query, QueryError> {
    let mut resolver = Resolver {
        collection,
        now_ms: options.now_ms.unwrap_or_else(time::now_ms),
        query: Query::new(Node::All),
        issues: Vec::new(),
        dropped: 0,
    };

    let defaults = &options.defaults;
    let given = [
        ("order", &defaults.order),
        ("direction", &defaults.direction),
        ("unique", &defaults.unique),
        ("prefer", &defaults.prefer),
    ];
    for (key, value) in given {
        if let Some(value) = value {
            let outcome = resolver.directive(key, value);
            resolver.note(outcome, None);
        }
    }
    for value in &defaults.include {
        let outcome = resolver.directive("include", value);
        resolver.note(outcome, None);
    }

    let filter = resolver.walk(&parse_cst(input)?, false);
    let Resolver { mut query, issues, dropped, .. } = resolver;
    if !issues.is_empty() {
        if !options.lenient || (dropped > 0 && filter.is_none()) {
            return Err(QueryError { issues });
        }
        query.warnings = issues;
    }
    if let Some(filter) = filter {
        query.filter = filter;
    }
    Ok(query)
}

struct Resolver<'a> {
    collection: &'a Collection,
    now_ms: i64,
    query: Query,
    issues: Vec<Issue>,
    /// Filter terms that failed to resolve. Failed directives are not counted.
    dropped: usize,
}

impl Resolver<'_> {
    /// Records a rejection as an issue at `span` and passes a success through.
    fn note<T>(&mut self, outcome: Result<T, Rejection>, span: Option<Span>) -> Option<T> {
        match outcome {
            Ok(value) => Some(value),
            Err(rejection) => {
                self.issues.push(Issue { code: rejection.code, message: rejection.message, span });
                None
            }
        }
    }

    /// Resolves a subtree. `None` means every term in it was a directive or was dropped.
    fn walk(&mut self, node: &Cst, negated: bool) -> Option<Node> {
        match node {
            Cst::And(children) | Cst::Or(children) => {
                let resolved: Vec<Node> =
                    children.iter().filter_map(|child| self.walk(child, negated)).collect();
                if resolved.is_empty() {
                    return None;
                }
                Some(if matches!(node, Cst::And(_)) { Node::and(resolved) } else { Node::or(resolved) })
            }
            Cst::Not(child) => self.walk(child, true).map(Node::not),
            Cst::Bare { value, exact, span } => {
                let op = if *exact { Op::Eq } else { Op::Colon };
                let field = self.collection.default_field().to_string();
                let coerced = coerce(self.collection, &field, op, value, self.now_ms);
                let resolved = self.note(coerced, Some(*span));
                if resolved.is_none() {
                    self.dropped += 1;
                }
                resolved.map(|value| Node::Cmp { field, op, value })
            }
            Cst::Term { key, op, value, span } => {
                let outcome = self.term(key, *op, value, negated);
                let failed = outcome.is_err();
                let resolved = self.note(outcome, Some(*span)).flatten();
                if failed && !DIRECTIVES.contains(&key.to_ascii_lowercase().as_str()) {
                    self.dropped += 1;
                }
                resolved
            }
        }
    }

    /// Resolves one `key op value` term. A directive resolves to no filter.
    fn term(&mut self, key: &str, op: Op, raw: &RawValue, negated: bool) -> Result<Option<Node>, Rejection> {
        let word = key.to_ascii_lowercase();
        let directive = DIRECTIVES.contains(&word.as_str());
        if directive || matches!(word.as_str(), "is" | "not" | "has") {
            if op != Op::Colon {
                return reject("invalid_operator", format!("\"{key}\" must be followed by \":\""));
            }
            if raw.kind == RawKind::Regex {
                return reject("invalid_value", format!("\"{key}:\" takes a name"));
            }
        }
        match word.as_str() {
            "is" | "not" => {
                let Some((name, _)) = self.collection.predicate(&raw.text) else {
                    return reject("unknown_predicate", format!("Unknown predicate \"{}\"", raw.text));
                };
                let node = Node::Is(name.to_string());
                Ok(Some(if word == "is" { node } else { Node::not(node) }))
            }
            "has" => Ok(Some(Node::Has(field_named(self.collection, &raw.text)?))),
            _ if directive => {
                if negated {
                    return reject("misplaced_directive", format!("\"{key}:\" cannot be negated"));
                }
                self.directive(&word, &raw.text)?;
                Ok(None)
            }
            _ => {
                let field = field_named(self.collection, key)?;
                let value = coerce(self.collection, &field, op, raw, self.now_ms)?;
                Ok(Some(Node::Cmp { field, op, value }))
            }
        }
    }

    fn directive(&mut self, key: &str, value: &str) -> Result<(), Rejection> {
        let unknown = |what: &str| format!("Unknown {what} \"{value}\"");
        match key {
            "order" | "sort" => match self.collection.order(value) {
                Some((name, _)) => self.query.order = Some(name.to_string()),
                None => return reject("unknown_order", unknown("order")),
            },
            "direction" | "dir" => {
                self.query.direction = match value.to_ascii_lowercase().as_str() {
                    "auto" => Direction::Auto,
                    "asc" => Direction::Asc,
                    "desc" => Direction::Desc,
                    _ => {
                        let message = format!("Direction must be asc, desc or auto, not \"{value}\"");
                        return reject("invalid_direction", message);
                    }
                }
            }
            "unique" => match self.collection.rollup(value) {
                Some((name, _)) => self.query.unique = Some(name.to_string()),
                None => return reject("unknown_rollup", unknown("rollup")),
            },
            "prefer" => match self.collection.preference(value) {
                Some((name, _)) => self.query.prefer = Some(name.to_string()),
                None => return reject("unknown_preference", unknown("preference")),
            },
            "include" => match self.collection.exclusion(value) {
                Some((name, _)) => {
                    if !self.query.include.iter().any(|included| included == name) {
                        self.query.include.push(name.to_string());
                    }
                }
                None => {
                    let message = format!("Nothing named \"{value}\" can be included");
                    return reject("unknown_inclusion", message);
                }
            },
            _ => unreachable!("not a directive: {key}"),
        }
        Ok(())
    }
}

fn field_named(collection: &Collection, name: &str) -> Result<String, Rejection> {
    match collection.resolve(name) {
        Some(field) => Ok(field.to_string()),
        None => reject("unknown_field", format!("Unknown field \"{name}\"")),
    }
}

fn member<'a>(values: &'a [String], text: &str) -> Option<&'a String> {
    values.iter().find(|value| value.eq_ignore_ascii_case(text))
}

/// Builds one comparison in code, held to the same rules as a parsed one.
///
/// `field` may be a name or an alias. A value written as a word may also name another
/// field of the same kind.
pub fn compare(collection: &Collection, field: &str, op: Op, value: &RawValue) -> Result<Node, QueryError> {
    let build = || {
        let field = field_named(collection, field)?;
        let value = coerce(collection, &field, op, value, time::now_ms())?;
        Ok(Node::Cmp { field, op, value })
    };
    build().map_err(|rejection: Rejection| QueryError::one(rejection.code, rejection.message))
}

/// Checks an operator and raw value against a field and returns the typed value.
pub(crate) fn coerce(
    collection: &Collection,
    name: &str,
    op: Op,
    raw: &RawValue,
    now_ms: i64,
) -> Result<Value, Rejection> {
    let field = collection.field(name);
    if !field.accepts(op) {
        let (op, kind) = (op.as_str(), field.kind.name());
        return reject(
            "invalid_operator",
            format!("Operator \"{op}\" is not valid for {kind} field \"{name}\""),
        );
    }
    let invalid = |expected: &str| -> Rejection {
        let message = format!("Field \"{name}\" expects {expected}, not \"{}\"", raw.text);
        Rejection { code: "invalid_value", message }
    };

    if raw.kind == RawKind::Regex {
        if field.kind != FieldKind::Text {
            let expected = match field.kind {
                FieldKind::Set => "set members".to_string(),
                kind => format!("a {} value", kind.name()),
            };
            return Err(invalid(&expected));
        }
        if !field.regex {
            return reject(
                "regex_not_allowed",
                format!("Field \"{name}\" does not accept regular expressions"),
            );
        }
        if op != Op::Colon {
            return reject("invalid_operator", "A regular expression must follow \":\"");
        }
        if let Err(error) = RegexBuilder::new(&raw.text).case_insensitive(true).build() {
            return reject("invalid_regex", format!("Invalid regular expression: {error}"));
        }
        return Ok(Value::Regex(raw.text.clone()));
    }

    // A bare word naming another field of the same kind compares the two fields.
    let reference = || {
        let other = collection.resolve(&raw.text).filter(|_| raw.kind == RawKind::Word)?;
        (collection.field(other).kind == field.kind).then(|| Value::Ref(other.to_string()))
    };
    let one_of = || format!("one of {}", field.values.join(", "));

    match field.kind {
        FieldKind::Text => Ok(Value::Text(raw.text.clone())),
        FieldKind::Keyword if field.values.is_empty() => Ok(Value::Keyword(raw.text.clone())),
        FieldKind::Keyword => match member(&field.values, &raw.text) {
            Some(value) => Ok(Value::Keyword(value.clone())),
            None => Err(invalid(&one_of())),
        },
        FieldKind::Number => match raw.text.trim().parse::<f64>() {
            Ok(value) if value.is_finite() && looks_numeric(&raw.text) => Ok(Value::Number(value)),
            _ => reference().ok_or_else(|| invalid("a number or a number field")),
        },
        FieldKind::Ordinal => match member(&field.values, &raw.text) {
            Some(value) => Ok(Value::Ordinal(value.clone())),
            None => Err(invalid(&one_of())),
        },
        FieldKind::Set => {
            if !raw.text.is_empty() && raw.text.bytes().all(|byte| byte.is_ascii_digit()) {
                return match raw.text.parse() {
                    Ok(count) => Ok(Value::Count(count)),
                    Err(_) => Err(invalid("a smaller count")),
                };
            }
            let mut chosen: Vec<&String> = Vec::new();
            for part in raw.text.split(',') {
                let nickname = field.nicknames.iter().find(|(key, _)| key.eq_ignore_ascii_case(part));
                match (nickname, member(&field.values, part)) {
                    (Some((_, members)), _) => chosen.extend(members),
                    (None, Some(value)) => chosen.push(value),
                    (None, None) => {
                        let expected = format!("a count or members of {}", field.values.join(", "));
                        return Err(invalid(&expected));
                    }
                }
            }
            let ordered = field.values.iter().filter(|value| chosen.contains(value)).cloned().collect();
            Ok(Value::Set(ordered))
        }
        FieldKind::Date => date_value(&raw.text, now_ms)
            .or_else(reference)
            .ok_or_else(|| invalid("a date (YYYY, YYYY-MM or YYYY-MM-DD), now, today or a date field")),
    }
}

/// Rules out the spellings `f64` parsing accepts beyond plain numbers, such as `inf`
/// and `nan`, which would otherwise shadow field names.
fn looks_numeric(text: &str) -> bool {
    text.trim().bytes().all(|byte| byte.is_ascii_digit() || b"+-.eE".contains(&byte))
}

fn date_value(text: &str, now_ms: i64) -> Option<Value> {
    let lower = text.to_ascii_lowercase();
    if lower == "now" {
        let instant = time::format_instant(now_ms);
        return Some(Value::Date { start: instant.clone(), end: instant, text: lower });
    }
    if lower == "today" {
        let day = time::day_of(now_ms);
        return Some(Value::Date {
            start: time::format_day(day),
            end: time::format_day(day + 1),
            text: lower,
        });
    }

    let parts: Vec<&str> = text.split('-').collect();
    let widths: Vec<usize> = parts.iter().map(|part| part.len()).collect();
    if !matches!(widths.as_slice(), [4] | [4, 2] | [4, 2, 2]) {
        return None;
    }
    if !parts.iter().all(|part| part.bytes().all(|byte| byte.is_ascii_digit())) {
        return None;
    }
    let year: i64 = parts[0].parse().ok()?;
    let month: Option<u32> = parts.get(1).and_then(|part| part.parse().ok());
    let day: Option<u32> = parts.get(2).and_then(|part| part.parse().ok());

    let start = time::days_from_civil(year, month.unwrap_or(1), day.unwrap_or(1));
    // An impossible date such as 02-30 runs on into the next month, so it does not
    // come back as the date that was written.
    if time::civil_from_days(start) != (year, month.unwrap_or(1), day.unwrap_or(1)) {
        return None;
    }
    let end = match (month, day) {
        (Some(_), Some(_)) => start + 1,
        (Some(12), None) | (None, _) => time::days_from_civil(year + 1, 1, 1),
        (Some(month), None) => time::days_from_civil(year, month + 1, 1),
    };
    Some(Value::Date { start: time::format_day(start), end: time::format_day(end), text: text.to_string() })
}
