//! A backend over rows held in memory. It is the reference implementation: every other
//! backend must return the same rows in the same order.

use std::cmp::Ordering;

use regex::RegexBuilder;
use serde_json::Value as Json;

use super::{Backend, Error, Page, PageRequest, Row};
use crate::ast::{Node, Query, Value};
use crate::collection::Collection;
use crate::cursor::{decode_cursor, encode_cursor};
use crate::plan::{SortKey, plan};
use crate::schema::{Field, FieldKind, Op};
use crate::semantics::{CompareOp, Effective, date_bounds, effective_op, ordinal_matches};

pub struct MemoryBackend {
    collection: Collection,
    rows: Vec<Row>,
}

impl MemoryBackend {
    pub fn new(collection: &Collection, rows: Vec<Row>) -> MemoryBackend {
        MemoryBackend { collection: collection.clone(), rows }
    }
}

impl Backend for MemoryBackend {
    fn search(&self, query: &Query, page: &PageRequest) -> Result<Page, Error> {
        let collection = &self.collection;
        let plan = plan(collection, query)?;
        let test = predicate(collection, &plan.filter);
        let mut hits: Vec<&Row> = self.rows.iter().filter(|row| test(row)).collect();

        if let Some(rollup) = &plan.rollup {
            // The first row of each group in preference order stands for the group.
            hits.sort_by(|a, b| compare_rows(collection, &rollup.prefer, a, b));
            let mut seen: Vec<&Json> = Vec::new();
            hits.retain(|row| {
                let group = row.get(&rollup.by).unwrap_or(&Json::Null);
                let first = !seen.contains(&group);
                if first {
                    seen.push(group);
                }
                first
            });
        }
        hits.sort_by(|a, b| compare_rows(collection, &plan.sort, a, b));
        let total = hits.len() as u64;

        if let Some(token) = &page.after {
            let values = decode_cursor(collection, &plan.sort, token)?;
            let position: Row = plan.sort.iter().map(|key| key.field.clone()).zip(values).collect();
            hits.retain(|row| compare_rows(collection, &plan.sort, row, &position) == Ordering::Greater);
        }
        let has_more = hits.len() > page.offset.saturating_add(page.limit);
        let rows: Vec<Row> = hits.into_iter().skip(page.offset).take(page.limit).cloned().collect();
        let next = match rows.last() {
            Some(last) if has_more => Some(encode_cursor(&plan.sort, last)),
            _ => None,
        };
        Ok(Page { rows, total, has_more, next })
    }
}

type Test<'a> = Box<dyn Fn(&Row) -> bool + 'a>;

fn cell<'r>(row: &'r Row, field: &str) -> Option<&'r Json> {
    row.get(field).filter(|value| !value.is_null())
}

/// Compiles a filter into a function that tests one row.
fn predicate<'a>(collection: &'a Collection, node: &'a Node) -> Test<'a> {
    match node {
        Node::All => Box::new(|_| true),
        Node::And(children) => {
            let tests: Vec<Test> = children.iter().map(|child| predicate(collection, child)).collect();
            Box::new(move |row| tests.iter().all(|test| test(row)))
        }
        Node::Or(children) => {
            let tests: Vec<Test> = children.iter().map(|child| predicate(collection, child)).collect();
            Box::new(move |row| tests.iter().any(|test| test(row)))
        }
        Node::Not(child) => {
            let test = predicate(collection, child);
            Box::new(move |row| !test(row))
        }
        Node::Is(name) => {
            let (_, filter) = collection.predicate(name).expect("a resolved predicate exists");
            predicate(collection, filter)
        }
        Node::Has(field) => match collection.field(field).kind {
            FieldKind::Set => Box::new(move |row| {
                cell(row, field).and_then(Json::as_array).is_some_and(|members| !members.is_empty())
            }),
            _ => Box::new(move |row| cell(row, field).is_some()),
        },
        Node::Cmp { field, op, value } => comparison(collection.field(field), field, *op, value),
    }
}

/// A null never satisfies a comparison, so `!=` is true of it.
fn comparison<'a>(field: &'a Field, name: &'a str, op: Op, value: &'a Value) -> Test<'a> {
    let op = match effective_op(field, op) {
        Effective::Ne => {
            let equal = comparison(field, name, Op::Eq, value);
            return Box::new(move |row| !equal(row));
        }
        Effective::Contains => None,
        Effective::Compare(op) => Some(op),
    };
    // Every arm below with a comparison operator is only reached with one.
    let compare = op.unwrap_or(CompareOp::Eq);
    let text = move |row: &'_ Row| cell(row, name).and_then(Json::as_str).map(str::to_string);

    match value {
        Value::Text(needle) => {
            let needle = needle.to_lowercase();
            if op.is_none() {
                Box::new(move |row| text(row).is_some_and(|cell| cell.to_lowercase().contains(&needle)))
            } else {
                Box::new(move |row| text(row).is_some_and(|cell| cell.to_lowercase() == needle))
            }
        }
        Value::Regex(source) => {
            let pattern = RegexBuilder::new(source)
                .case_insensitive(true)
                .build()
                .expect("a resolved regular expression compiles");
            Box::new(move |row| text(row).is_some_and(|cell| pattern.is_match(&cell)))
        }
        Value::Keyword(wanted) => Box::new(move |row| text(row).is_some_and(|cell| cell == *wanted)),
        Value::Number(wanted) => Box::new(move |row| {
            let number = cell(row, name).and_then(Json::as_f64);
            number.and_then(|cell| cell.partial_cmp(wanted)).is_some_and(|order| compare.holds(order))
        }),
        Value::Ordinal(wanted) => {
            let matches = ordinal_matches(field, compare, wanted);
            Box::new(move |row| text(row).is_some_and(|cell| matches.contains(&cell.as_str())))
        }
        Value::Count(wanted) => Box::new(move |row| {
            let size = cell(row, name).and_then(Json::as_array).map(|members| members.len() as u64);
            size.is_some_and(|size| compare.holds(size.cmp(wanted)))
        }),
        Value::Set(wanted) => Box::new(move |row| {
            let Some(members) = cell(row, name).and_then(Json::as_array) else {
                return false;
            };
            let members: Vec<&str> = members.iter().filter_map(Json::as_str).collect();
            let has_all = wanted.iter().all(|member| members.contains(&member.as_str()));
            let within = members.iter().all(|member| wanted.iter().any(|wanted| wanted == member));
            match compare {
                CompareOp::Ge => has_all,
                CompareOp::Le => within,
                CompareOp::Eq => has_all && within,
                CompareOp::Gt => has_all && !within,
                CompareOp::Lt => within && !has_all,
            }
        }),
        Value::Date { start, end, .. } => {
            let bounds = date_bounds(compare, start, end);
            Box::new(move |row| {
                text(row)
                    .is_some_and(|cell| bounds.iter().all(|(op, bound)| op.holds(cell.as_str().cmp(bound))))
            })
        }
        Value::Ref(other) => Box::new(move |row| match (cell(row, name), cell(row, other)) {
            (Some(left), Some(right)) => compare.holds(compare_cells(field, left, right)),
            _ => false,
        }),
    }
}

/// Orders two non-null cells of a field: numbers by value, ordinals by rank, and
/// everything else as strings, byte by byte.
fn compare_cells(field: &Field, a: &Json, b: &Json) -> Ordering {
    match field.kind {
        FieldKind::Number => {
            let (a, b) = (a.as_f64().unwrap_or(f64::NAN), b.as_f64().unwrap_or(f64::NAN));
            a.partial_cmp(&b).unwrap_or(Ordering::Equal)
        }
        FieldKind::Ordinal => {
            let rank = |value: &Json| value.as_str().and_then(|member| field.rank(member));
            rank(a).cmp(&rank(b))
        }
        _ => a.as_str().cmp(&b.as_str()),
    }
}

/// Orders rows by the given keys. Nulls go where each key says, whatever its direction.
fn compare_rows(collection: &Collection, keys: &[SortKey], a: &Row, b: &Row) -> Ordering {
    for key in keys {
        let order = match (cell(a, &key.field), cell(b, &key.field)) {
            (None, None) => Ordering::Equal,
            (None, Some(_)) if key.nulls_first => Ordering::Less,
            (None, Some(_)) => Ordering::Greater,
            (Some(_), None) if key.nulls_first => Ordering::Greater,
            (Some(_), None) => Ordering::Less,
            (Some(x), Some(y)) => {
                let order = compare_cells(collection.field(&key.field), x, y);
                if key.descending { order.reverse() } else { order }
            }
        };
        if order != Ordering::Equal {
            return order;
        }
    }
    Ordering::Equal
}
