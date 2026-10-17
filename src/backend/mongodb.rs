//! MongoDB.
//!
//! A filter compiles to a query document, and a search runs as an aggregation pipeline.
//!
//! How the documents must be shaped: a set field is an array of strings, a number field
//! any numeric type, and a date field either a BSON date or an ISO-8601 string. The
//! collection's id is an ordinary field of that name, not `_id`, unless it is given
//! `_id` as its column.
//!
//! Things that follow MongoDB and not this crate:
//!
//! - **A BSON date keeps milliseconds only**, so two rows closer together than that
//!   are told apart by id alone.
//! - **Regular expressions are matched by MongoDB's engine**, after being checked
//!   against this crate's syntax when the query is parsed.
//! - **The driver is asynchronous underneath.** Call the backend from a blocking
//!   context, never straight from an async task.
//! - **Sorting on a nullable or ordinal field, and comparing two fields, are computed
//!   per document**, which no index can speed up.

use std::collections::HashMap;

use mongodb::bson::{Bson, DateTime, Document, doc};
use mongodb::sync::Database;
use regex::escape;
use serde_json::{Value as Json, json};

use super::{Backend, Error, Page, PageRequest, Row};
use crate::ast::{Node, Query, Value};
use crate::collection::{Collection, DefinitionError};
use crate::cursor::{decode_cursor, encode_cursor};
use crate::plan::{SortKey, plan};
use crate::schema::{FieldKind, Op};
use crate::semantics::{CompareOp, Effective, date_bounds, effective_op, ordinal_matches};
use crate::time;

/// How a date field is stored.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum DateStorage {
    /// A BSON date. Rows come back as UTC ISO timestamps with six fractional digits,
    /// of which the last three are always zero.
    #[default]
    Date,
    /// ISO-8601 UTC strings, compared as text. Calendar days are stored this way, as
    /// `YYYY-MM-DD`.
    Text,
}

/// How a collection's fields are stored in MongoDB.
#[derive(Clone, Debug, Default)]
pub struct MongoOptions {
    dates: DateStorage,
    date_columns: HashMap<String, DateStorage>,
}

impl MongoOptions {
    /// The storage of date fields that are not given their own.
    pub fn dates(mut self, storage: DateStorage) -> Self {
        self.dates = storage;
        self
    }

    /// The storage of one date field, by field name.
    pub fn date_column(mut self, field: impl Into<String>, storage: DateStorage) -> Self {
        self.date_columns.insert(field.into(), storage);
        self
    }

    /// Checks that every field given its own storage exists and is a date field.
    fn check(&self, collection: &Collection) -> Result<(), DefinitionError> {
        for field in self.date_columns.keys() {
            if !collection.fields().any(|(name, declared)| name == field && declared.kind == FieldKind::Date)
            {
                return Err(DefinitionError(format!(
                    "Collection \"{}\": \"{field}\" is not a date field",
                    collection.name()
                )));
            }
        }
        Ok(())
    }
}

fn all(parts: Vec<Document>) -> Document {
    doc! { "$and": parts }
}

fn any(parts: Vec<Document>) -> Document {
    doc! { "$or": parts }
}

fn not(part: Document) -> Document {
    doc! { "$nor": [part] }
}

fn nothing() -> Document {
    not(doc! {})
}

fn operator(op: CompareOp) -> &'static str {
    match op {
        CompareOp::Eq => "$eq",
        CompareOp::Lt => "$lt",
        CompareOp::Le => "$lte",
        CompareOp::Gt => "$gt",
        CompareOp::Ge => "$gte",
    }
}

fn number(value: f64) -> Bson {
    if value.fract() == 0.0 && value.abs() < 9e15 { Bson::Int64(value as i64) } else { Bson::Double(value) }
}

struct Compiler<'a> {
    collection: &'a Collection,
    options: &'a MongoOptions,
}

impl Compiler<'_> {
    /// The name of a field in the documents.
    fn name(&self, field: &str) -> String {
        self.collection.field(field).column.clone().unwrap_or_else(|| field.to_string())
    }

    fn date_of(&self, field: &str) -> DateStorage {
        *self.options.date_columns.get(field).unwrap_or(&self.options.dates)
    }

    /// An ISO date or timestamp as the value a date field is compared with.
    fn date(&self, field: &str, iso: &str) -> Bson {
        match (self.date_of(field), time::parse_iso_ms(iso)) {
            (DateStorage::Date, Some(ms)) => Bson::DateTime(DateTime::from_millis(ms)),
            _ => Bson::String(iso.to_string()),
        }
    }

    /// A field that is neither null nor missing.
    fn present(&self, field: &str) -> Document {
        doc! { self.name(field): { "$ne": Bson::Null } }
    }

    /// A field that is null or missing.
    fn absent(&self, field: &str) -> Document {
        doc! { self.name(field): Bson::Null }
    }

    fn filter(&self, node: &Node) -> Document {
        match node {
            Node::All => doc! {},
            Node::And(children) => all(children.iter().map(|child| self.filter(child)).collect()),
            Node::Or(children) => any(children.iter().map(|child| self.filter(child)).collect()),
            Node::Not(child) => not(self.filter(child)),
            Node::Is(name) => {
                let (_, filter) = self.collection.predicate(name).expect("a resolved predicate exists");
                self.filter(filter)
            }
            Node::Has(field) => match self.collection.field(field).kind {
                FieldKind::Set => doc! { format!("{}.0", self.name(field)): { "$exists": true } },
                _ => self.present(field),
            },
            Node::Cmp { field, op, value } => self.comparison(field, *op, value),
        }
    }

    /// A null or missing field never satisfies a comparison. Negation is `$nor`, which
    /// such a field does satisfy, as everywhere else.
    fn comparison(&self, name: &str, op: Op, value: &Value) -> Document {
        let field = self.collection.field(name);
        let path = self.name(name);
        let op = match effective_op(field, op) {
            Effective::Ne => return not(self.comparison(name, Op::Eq, value)),
            Effective::Contains => None,
            Effective::Compare(op) => Some(op),
        };
        // Every arm below with a comparison operator is only reached with one.
        let compare = op.unwrap_or(CompareOp::Eq);

        match value {
            // The text is matched as itself, so everything a pattern would read is escaped.
            Value::Text(text) if op.is_none() => doc! { path: { "$regex": escape(text), "$options": "i" } },
            Value::Text(text) => doc! { path: { "$regex": format!("^{}$", escape(text)), "$options": "i" } },
            Value::Regex(source) => doc! { path: { "$regex": source, "$options": "i" } },
            Value::Keyword(wanted) => doc! { path: wanted },
            Value::Number(wanted) => doc! { path: { operator(compare): number(*wanted) } },
            Value::Ordinal(wanted) => {
                let matches = ordinal_matches(field, compare, wanted);
                if matches.is_empty() {
                    nothing()
                } else {
                    doc! { path: { "$in": matches } }
                }
            }
            Value::Count(wanted) => {
                let size = doc! { "$size": { "$ifNull": [format!("${path}"), []] } };
                doc! { "$expr": { operator(compare): [size, *wanted as i64] } }
            }
            Value::Set(wanted) => {
                let has_all = || doc! { &path: { "$all": wanted } };
                let within = || doc! { &path: { "$not": { "$elemMatch": { "$nin": wanted } } } };
                match compare {
                    CompareOp::Ge => has_all(),
                    CompareOp::Le => within(),
                    CompareOp::Eq => all(vec![has_all(), within()]),
                    CompareOp::Gt => all(vec![has_all(), not(within())]),
                    CompareOp::Lt => all(vec![within(), not(has_all())]),
                }
            }
            Value::Date { start, end, .. } => {
                let mut bounds = Document::new();
                for (op, bound) in date_bounds(compare, start, end) {
                    bounds.insert(operator(op), self.date(name, bound));
                }
                doc! { path: bounds }
            }
            Value::Ref(other) => {
                // Two date fields stored differently are both read as dates.
                let mixed = field.kind == FieldKind::Date && self.date_of(name) != self.date_of(other);
                let operand = |field: &str| {
                    let path = format!("${}", self.name(field));
                    if mixed { Bson::Document(doc! { "$toDate": path }) } else { Bson::String(path) }
                };
                // Compared in an expression, a null would count as less than any value.
                all(vec![
                    self.present(name),
                    self.present(other),
                    doc! { "$expr": { operator(compare): [operand(name), operand(other)] } },
                ])
            }
        }
    }

    /// Stages that sort by the given keys. A nullable field first sorts on whether it
    /// is null, and an ordinal on its rank, both computed into fields named after `tag`.
    fn sort(&self, keys: &[SortKey], tag: &str) -> Vec<Document> {
        let mut computed = Document::new();
        let mut order = Document::new();
        for (index, key) in keys.iter().enumerate() {
            let field = self.collection.field(&key.field);
            let path = self.name(&key.field);
            let direction = if key.descending { -1 } else { 1 };
            if field.nullable {
                // Named so the macro below does not read the first as a BSON null.
                let (if_null, if_value) = if key.nulls_first { (0, 1) } else { (1, 0) };
                let is_null = doc! { "$eq": [{ "$ifNull": [format!("${path}"), Bson::Null] }, Bson::Null] };
                computed.insert(format!("__{tag}{index}n"), doc! { "$cond": [is_null, if_null, if_value] });
                order.insert(format!("__{tag}{index}n"), 1);
            }
            if field.kind == FieldKind::Ordinal {
                let rank = doc! { "$indexOfArray": [&field.values, format!("${path}")] };
                computed.insert(format!("__{tag}{index}r"), rank);
                order.insert(format!("__{tag}{index}r"), direction);
            } else {
                order.insert(path, direction);
            }
        }
        let mut stages = Vec::new();
        if !computed.is_empty() {
            stages.push(doc! { "$addFields": computed });
        }
        stages.push(doc! { "$sort": order });
        stages
    }

    /// A cursor's value for a field, as the value its documents hold.
    fn cursor_value(&self, name: &str, value: &Json) -> Bson {
        match (self.collection.field(name).kind, value) {
            (FieldKind::Date, Json::String(date)) => self.date(name, date),
            (_, Json::Number(wanted)) => number(wanted.as_f64().unwrap_or_default()),
            (_, Json::String(text)) => Bson::String(text.clone()),
            _ => Bson::Null,
        }
    }

    /// The documents that sort strictly after a cursor: later on the first key, or
    /// level on it and after the cursor on the keys that remain.
    fn after(&self, keys: &[SortKey], values: &[Json]) -> Document {
        let (key, rest) = keys.split_first().expect("a sort has at least the id");
        let value = &values[0];
        let name = key.field.as_str();
        let field = self.collection.field(name);
        let path = self.name(name);
        let sent = self.cursor_value(name, value);

        // Nulls sit at one end whatever the direction, so they are handled apart from values.
        let mut later: Option<Document> = None;
        if value.is_null() {
            if key.nulls_first {
                later = Some(self.present(name));
            }
        } else {
            if field.kind == FieldKind::Ordinal {
                let rank = value.as_str().and_then(|member| field.rank(member)).unwrap_or(0);
                let beyond: Vec<&String> = field
                    .values
                    .iter()
                    .enumerate()
                    .filter(|(index, _)| if key.descending { *index < rank } else { *index > rank })
                    .map(|(_, member)| member)
                    .collect();
                if !beyond.is_empty() {
                    later = Some(doc! { &path: { "$in": beyond } });
                }
            } else {
                let op = if key.descending { "$lt" } else { "$gt" };
                later = Some(doc! { &path: { op: sent.clone() } });
            }
            if field.nullable && !key.nulls_first {
                later = Some(match later {
                    Some(later) => any(vec![later, self.absent(name)]),
                    None => self.absent(name),
                });
            }
        }
        if rest.is_empty() {
            return later.unwrap_or_else(nothing);
        }

        let level = if value.is_null() {
            self.absent(name)
        } else {
            doc! { &path: sent }
        };
        let then = all(vec![level, self.after(rest, &values[1..])]);
        match later {
            Some(later) => any(vec![later, then]),
            None => then,
        }
    }

    /// One document as a row.
    fn row(&self, document: &Document) -> Row {
        let mut row = Row::new();
        for (name, field) in self.collection.fields() {
            let value = match document.get(self.name(name)) {
                None | Some(Bson::Null) => Json::Null,
                Some(Bson::DateTime(date)) => {
                    // Three more digits keep the form every other backend returns.
                    json!(time::format_instant(date.timestamp_millis()).replace('Z', "000Z"))
                }
                Some(Bson::Int32(value)) => json!(value),
                Some(Bson::Int64(value)) => json!(value),
                Some(Bson::Double(value)) if value.fract() == 0.0 && value.abs() < 9e15 => {
                    json!(*value as i64)
                }
                Some(Bson::Double(value)) => json!(value),
                Some(Bson::Decimal128(value)) => {
                    value.to_string().parse::<f64>().map_or(Json::Null, |number| json!(number))
                }
                Some(Bson::String(text)) => json!(text),
                Some(Bson::Array(members)) => {
                    members.iter().filter_map(|member| member.as_str().map(|text| json!(text))).collect()
                }
                Some(other) => json!(other.to_string()),
            };
            let value = if field.kind == FieldKind::Set && value.is_null() { json!([]) } else { value };
            row.insert(name.to_string(), value);
        }
        row
    }
}

/// A backend over one MongoDB collection.
pub struct MongoBackend {
    collection: Collection,
    documents: mongodb::sync::Collection<Document>,
    options: MongoOptions,
}

impl MongoBackend {
    /// The MongoDB collection being searched, for work outside the backend such as
    /// loading documents.
    pub fn documents(&self) -> &mongodb::sync::Collection<Document> {
        &self.documents
    }
}

impl Backend for MongoBackend {
    fn search(&self, query: &Query, page: &PageRequest) -> Result<Page, Error> {
        let failed = |error: mongodb::error::Error| Error::Backend(error.to_string());
        let collection = &self.collection;
        let plan = plan(collection, query)?;
        let compiler = Compiler { collection, options: &self.options };
        let matching = compiler.filter(&plan.filter);

        let mut pipeline = vec![doc! { "$match": matching.clone() }];
        let total = match &plan.rollup {
            None => self.documents.count_documents(matching).run().map_err(failed)?,
            Some(rollup) => {
                // The first document of each group, in preference order, stands for it.
                let group = format!("${}", compiler.name(&rollup.by));
                pipeline.extend(compiler.sort(&rollup.prefer, "p"));
                pipeline.push(doc! { "$group": { "_id": &group, "kept": { "$first": "$$ROOT" } } });
                pipeline.push(doc! { "$replaceRoot": { "newRoot": "$kept" } });

                let counting = vec![
                    doc! { "$match": matching },
                    doc! { "$group": { "_id": &group } },
                    doc! { "$count": "n" },
                ];
                let mut counted = self.documents.aggregate(counting).run().map_err(failed)?;
                match counted.next().transpose().map_err(failed)? {
                    Some(count) => count.get("n").and_then(Bson::as_i32).map_or(0, |n| n as u64),
                    None => 0,
                }
            }
        };

        if let Some(token) = &page.after {
            let values = decode_cursor(collection, &plan.sort, token)?;
            pipeline.push(doc! { "$match": compiler.after(&plan.sort, &values) });
        }
        pipeline.extend(compiler.sort(&plan.sort, "s"));
        pipeline.push(doc! { "$skip": i64::try_from(page.offset).unwrap_or(i64::MAX) });
        // One row more than asked for shows whether another page follows.
        pipeline.push(doc! { "$limit": i64::try_from(page.limit).unwrap_or(i64::MAX - 1).saturating_add(1) });

        let found = self.documents.aggregate(pipeline).allow_disk_use(true).run().map_err(failed)?;
        let mut rows = Vec::new();
        for document in found {
            rows.push(compiler.row(&document.map_err(failed)?));
        }
        let has_more = rows.len() > page.limit;
        rows.truncate(page.limit);
        let next = match rows.last() {
            Some(last) if has_more => Some(encode_cursor(&plan.sort, last)),
            _ => None,
        };
        Ok(Page { rows, total, has_more, next })
    }
}

/// A backend over the MongoDB collection named after the collection, in a database.
pub fn mongo_backend(
    collection: &Collection,
    database: &Database,
    options: MongoOptions,
) -> Result<MongoBackend, DefinitionError> {
    options.check(collection)?;
    let documents = database.collection::<Document>(collection.name());
    Ok(MongoBackend { collection: collection.clone(), documents, options })
}
