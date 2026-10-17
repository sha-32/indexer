//! Elasticsearch and OpenSearch.
//!
//! A filter compiles to their query language directly, and one request returns both the
//! page and the count of all matches.
//!
//! How the index must be mapped:
//!
//! - **text**: `keyword`. Matching is by substring, as in every other backend, not by
//!   analysed words. On Elasticsearch a `wildcard` field does the same faster where the
//!   values are long. On OpenSearch a `wildcard` field cannot be sorted and paged on.
//! - **keyword**, **ordinal**: `keyword`.
//! - **number**: any numeric type.
//! - **set**: `keyword`, holding an array.
//! - **date**: `date` or `date_nanos`, or `keyword` holding ISO-8601 text. A plain `date`
//!   keeps milliseconds only, so two rows closer together than that are told apart by
//!   id alone; use `date_nanos` where that matters.
//!
//! What these engines cannot do the way the others do:
//!
//! - **Regular expressions are refused.** Theirs is a different language, anchored and
//!   without the usual classes, so a pattern would not mean what it says.
//! - **A rollup (`unique:`) is done in two requests** and is limited to a set number of
//!   groups, because the engine has no way to pick each group's row by one order and
//!   page the groups by another.
//! - **An offset cannot reach past the index's result window**, 10,000 rows by default.
//!   A cursor can.
//! - **Comparing two fields** runs a script per row, which no index can speed up.

use std::collections::HashMap;

use serde_json::{Value as Json, json};
use ureq::Agent;

use super::{Backend, Error, Page, PageRequest, Row};
use crate::ast::{Node, Query, QueryError, Value};
use crate::collection::{Collection, DefinitionError};
use crate::cursor::{decode_cursor, encode_cursor};
use crate::plan::{SortKey, plan};
use crate::schema::{Field, FieldKind, Op};
use crate::semantics::{CompareOp, Effective, date_bounds, effective_op, ordinal_matches};

/// How a date field is mapped.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum DateStorage {
    /// A `date` or `date_nanos` holding an instant. Rows come back as UTC ISO
    /// timestamps with six fractional digits.
    #[default]
    Instant,
    /// A `date` holding calendar days. Literals and `now` are compared by their UTC
    /// day, and rows come back as `YYYY-MM-DD`.
    Day,
    /// A `keyword` holding ISO-8601 UTC strings, compared as text.
    Text,
}

/// How a collection's fields are mapped in the index.
#[derive(Clone, Debug)]
pub struct ElasticOptions {
    dates: DateStorage,
    date_columns: HashMap<String, DateStorage>,
    max_groups: usize,
}

impl Default for ElasticOptions {
    fn default() -> ElasticOptions {
        ElasticOptions { dates: DateStorage::default(), date_columns: HashMap::new(), max_groups: 10_000 }
    }
}

impl ElasticOptions {
    /// The mapping of date fields that are not given their own.
    pub fn dates(mut self, storage: DateStorage) -> Self {
        self.dates = storage;
        self
    }

    /// The mapping of one date field, by field name.
    pub fn date_column(mut self, field: impl Into<String>, storage: DateStorage) -> Self {
        self.date_columns.insert(field.into(), storage);
        self
    }

    /// The most groups a rollup may produce before the search is refused.
    pub fn max_groups(mut self, groups: usize) -> Self {
        self.max_groups = groups;
        self
    }

    /// Checks that every field given its own mapping exists and is a date field.
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

/// How to reach an Elasticsearch or OpenSearch cluster.
#[derive(Clone, Debug)]
pub struct ElasticClient {
    url: String,
    headers: Vec<(String, String)>,
    agent: Agent,
}

impl ElasticClient {
    /// A client for the cluster at a URL such as `http://localhost:9200`.
    pub fn new(url: impl Into<String>) -> ElasticClient {
        // An error status carries the cluster's explanation in its body, which is
        // wanted, so the status is checked here and not by the HTTP client.
        let agent = Agent::config_builder().http_status_as_error(false).build().into();
        ElasticClient { url: url.into().trim_end_matches('/').to_string(), headers: Vec::new(), agent }
    }

    /// A header sent with every request, such as `Authorization: ApiKey ...`.
    pub fn header(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.headers.push((name.into(), value.into()));
        self
    }

    /// Sends one request and returns the parsed response. A body is sent as JSON.
    pub fn request(&self, method: &str, path: &str, body: Option<&Json>) -> Result<Json, String> {
        let url = format!("{}/{}", self.url, path.trim_start_matches('/'));
        let request = ureq::http::Request::builder().method(method).uri(&url);
        let request = self.headers.iter().fold(request, |request, (name, value)| request.header(name, value));
        let sent = match body {
            Some(body) => {
                let request = request.header("Content-Type", "application/json").body(body.to_string());
                self.agent.run(request.map_err(|error| error.to_string())?)
            }
            None => self.agent.run(request.body(()).map_err(|error| error.to_string())?),
        };
        let mut response = sent.map_err(|error| error.to_string())?;
        let text = response.body_mut().read_to_string().map_err(|error| error.to_string())?;
        let parsed: Json = serde_json::from_str(&text).unwrap_or(Json::Null);
        if response.status().is_success() {
            return Ok(parsed);
        }
        let reason = &parsed["error"]["root_cause"][0]["reason"];
        Err(reason
            .as_str()
            .or(parsed["error"]["reason"].as_str())
            .map_or(text.trim().to_string(), str::to_string))
    }
}

const ISO_MICROS: &str = "yyyy-MM-dd'T'HH:mm:ss.SSSSSS'Z'";
/// Reads an ISO date or timestamp with any number of fractional digits.
const ISO_ANY: &str = "strict_date_optional_time_nanos";

fn all(parts: Vec<Json>) -> Json {
    json!({ "bool": { "filter": parts } })
}

fn any(parts: Vec<Json>) -> Json {
    json!({ "bool": { "should": parts, "minimum_should_match": 1 } })
}

fn not(part: Json) -> Json {
    json!({ "bool": { "must_not": [part] } })
}

fn nothing() -> Json {
    json!({ "match_none": {} })
}

fn unsupported(code: &'static str, message: &str) -> QueryError {
    QueryError::one(code, message)
}

/// The script operator for a comparison.
fn script_op(op: CompareOp) -> &'static str {
    match op {
        CompareOp::Eq => "==",
        other => other.as_str(),
    }
}

struct Compiler<'a> {
    collection: &'a Collection,
    options: &'a ElasticOptions,
}

impl Compiler<'_> {
    /// The name of a field in the index.
    fn name(&self, field: &str) -> String {
        self.collection.field(field).column.clone().unwrap_or_else(|| field.to_string())
    }

    fn date_of(&self, field: &str) -> DateStorage {
        *self.options.date_columns.get(field).unwrap_or(&self.options.dates)
    }

    fn exists(&self, field: &str) -> Json {
        json!({ "exists": { "field": self.name(field) } })
    }

    fn range(&self, field: &str, bounds: &[(CompareOp, Json)]) -> Json {
        let mut range = serde_json::Map::new();
        for (op, value) in bounds {
            let keys: &[&str] = match op {
                CompareOp::Eq => &["gte", "lte"],
                CompareOp::Lt => &["lt"],
                CompareOp::Le => &["lte"],
                CompareOp::Gt => &["gt"],
                CompareOp::Ge => &["gte"],
            };
            for key in keys {
                range.insert((*key).to_string(), value.clone());
            }
        }
        if self.collection.field(field).kind == FieldKind::Date && self.date_of(field) == DateStorage::Instant
        {
            range.insert("format".to_string(), json!(ISO_ANY));
        }
        json!({ "range": { self.name(field): range } })
    }

    /// A date bound in the form the field's mapping compares by.
    fn date(&self, field: &str, value: &str) -> Json {
        match self.date_of(field) {
            DateStorage::Day => json!(value.chars().take(10).collect::<String>()),
            _ => json!(value),
        }
    }

    fn script(&self, source: String, params: Json) -> Json {
        json!({ "script": { "script": { "source": source, "params": params } } })
    }

    fn filter(&self, node: &Node) -> Result<Json, QueryError> {
        Ok(match node {
            Node::All => json!({ "match_all": {} }),
            Node::And(children) => {
                all(children.iter().map(|child| self.filter(child)).collect::<Result<_, _>>()?)
            }
            Node::Or(children) => {
                any(children.iter().map(|child| self.filter(child)).collect::<Result<_, _>>()?)
            }
            Node::Not(child) => not(self.filter(child)?),
            Node::Is(name) => {
                let (_, filter) = self.collection.predicate(name).expect("a resolved predicate exists");
                self.filter(filter)?
            }
            Node::Has(field) => self.exists(field),
            Node::Cmp { field, op, value } => self.comparison(field, *op, value)?,
        })
    }

    /// A null, which is a missing field here, never satisfies a comparison. Negation
    /// is `must_not`, which a missing field does satisfy, as everywhere else.
    fn comparison(&self, name: &str, op: Op, value: &Value) -> Result<Json, QueryError> {
        let field = self.collection.field(name);
        let es = self.name(name);
        let op = match effective_op(field, op) {
            Effective::Ne => return Ok(not(self.comparison(name, Op::Eq, value)?)),
            Effective::Contains => None,
            Effective::Compare(op) => Some(op),
        };
        // Every arm below with a comparison operator is only reached with one.
        let compare = op.unwrap_or(CompareOp::Eq);

        Ok(match value {
            Value::Text(text) if op.is_none() && text.is_empty() => self.exists(name),
            Value::Text(text) if op.is_none() => {
                let mut pattern = String::from("*");
                for ch in text.chars() {
                    if matches!(ch, '*' | '?' | '\\') {
                        pattern.push('\\');
                    }
                    pattern.push(ch);
                }
                pattern.push('*');
                json!({ "wildcard": { es: { "value": pattern, "case_insensitive": true } } })
            }
            Value::Text(text) => json!({ "term": { es: { "value": text, "case_insensitive": true } } }),
            Value::Regex(_) => {
                return Err(unsupported(
                    "unsupported_regex",
                    "Regular expressions are not available for this collection",
                ));
            }
            Value::Keyword(wanted) => json!({ "term": { es: wanted } }),
            Value::Number(wanted) => self.range(name, &[(compare, json!(wanted))]),
            Value::Ordinal(wanted) => {
                let matches = ordinal_matches(field, compare, wanted);
                if matches.is_empty() { nothing() } else { json!({ "terms": { es: matches } }) }
            }
            Value::Count(wanted) => self.script(
                format!("doc[params.f].size() {} params.n", script_op(compare)),
                json!({ "f": es, "n": wanted }),
            ),
            Value::Set(wanted) => {
                let has_all =
                    || all(wanted.iter().map(|member| json!({ "term": { &es: member } })).collect());
                // Nothing outside the wanted members means none of the other members
                // the field could hold.
                let others: Vec<&String> =
                    field.values.iter().filter(|member| !wanted.contains(member)).collect();
                let within = || not(json!({ "terms": { &es: others } }));
                match compare {
                    CompareOp::Ge => has_all(),
                    CompareOp::Le => within(),
                    CompareOp::Eq => all(vec![has_all(), within()]),
                    CompareOp::Gt => all(vec![has_all(), not(within())]),
                    CompareOp::Lt => all(vec![within(), not(has_all())]),
                }
            }
            Value::Date { start, end, .. } => {
                let bounds: Vec<(CompareOp, Json)> = date_bounds(compare, start, end)
                    .into_iter()
                    .map(|(op, bound)| (op, self.date(name, bound)))
                    .collect();
                self.range(name, &bounds)
            }
            Value::Ref(other) => {
                let both = "doc[params.a].size() > 0 && doc[params.b].size() > 0";
                let op = script_op(compare);
                let text = |field: &str| self.date_of(field) == DateStorage::Text;
                let test = match field.kind {
                    FieldKind::Date if text(name) != text(other) => {
                        return Err(unsupported(
                            "unsupported_comparison",
                            "These two date fields are stored too differently to compare",
                        ));
                    }
                    FieldKind::Date if text(name) => {
                        format!("doc[params.a].value.compareTo(doc[params.b].value) {op} 0")
                    }
                    FieldKind::Date => format!(
                        "doc[params.a].value.toInstant().compareTo(doc[params.b].value.toInstant()) {op} 0"
                    ),
                    _ => format!("doc[params.a].value {op} doc[params.b].value"),
                };
                self.script(format!("{both} && {test}"), json!({ "a": es, "b": self.name(other) }))
            }
        })
    }

    fn sort(&self, keys: &[SortKey]) -> Json {
        keys.iter()
            .map(|key| {
                let field = self.collection.field(&key.field);
                let order = if key.descending { "desc" } else { "asc" };
                if field.kind != FieldKind::Ordinal {
                    let missing = if key.nulls_first { "_first" } else { "_last" };
                    return json!({ self.name(&key.field): { "order": order, "missing": missing } });
                }
                // An ordinal sorts by rank. A row without one is given a rank beyond
                // either end, on whichever side puts it where nulls belong.
                let ranks: serde_json::Map<String, Json> =
                    field.values.iter().enumerate().map(|(rank, member)| (member.clone(), json!(rank))).collect();
                let missing = if key.nulls_first == key.descending { field.values.len() as i64 } else { -1 };
                json!({ "_script": {
                    "type": "number",
                    "order": order,
                    "script": {
                        "source": "doc[params.f].size() == 0 ? params.missing : params.ranks[doc[params.f].value]",
                        "params": { "f": self.name(&key.field), "ranks": ranks, "missing": missing },
                    },
                } })
            })
            .collect()
    }

    /// A cursor's value for a field, in the form its mapping compares by.
    fn cursor_value(&self, field: &Field, name: &str, value: &Json) -> Json {
        match (field.kind, value) {
            (FieldKind::Date, Json::String(date)) => self.date(name, date),
            _ => value.clone(),
        }
    }

    /// The rows that sort strictly after a cursor: later on the first key, or level on
    /// it and after the cursor on the keys that remain.
    fn after(&self, keys: &[SortKey], values: &[Json]) -> Json {
        let (key, rest) = keys.split_first().expect("a sort has at least the id");
        let value = &values[0];
        let name = key.field.as_str();
        let field = self.collection.field(name);
        let es = self.name(name);
        let sent = self.cursor_value(field, name, value);

        // Nulls sit at one end whatever the direction, so they are handled apart from values.
        let mut later: Option<Json> = None;
        if value.is_null() {
            if key.nulls_first {
                later = Some(self.exists(name));
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
                    later = Some(json!({ "terms": { &es: beyond } }));
                }
            } else {
                let op = if key.descending { CompareOp::Lt } else { CompareOp::Gt };
                later = Some(self.range(name, &[(op, sent.clone())]));
            }
            if field.nullable && !key.nulls_first {
                let missing = not(self.exists(name));
                later = Some(match later {
                    Some(later) => any(vec![later, missing]),
                    None => missing,
                });
            }
        }
        if rest.is_empty() {
            return later.unwrap_or_else(nothing);
        }

        let level = if value.is_null() {
            not(self.exists(name))
        } else if matches!(field.kind, FieldKind::Number | FieldKind::Date) {
            self.range(name, &[(CompareOp::Eq, sent)])
        } else {
            json!({ "term": { &es: sent } })
        };
        let then = all(vec![level, self.after(rest, &values[1..])]);
        match later {
            Some(later) => any(vec![later, then]),
            None => then,
        }
    }

    /// One hit as a row.
    fn row(&self, hit: &Json) -> Row {
        let mut row = Row::new();
        for (name, field) in self.collection.fields() {
            let es = self.name(name);
            let source = &hit["_source"][&es];
            let value = match field.kind {
                FieldKind::Date if self.date_of(name) != DateStorage::Text => hit["fields"][&es][0].clone(),
                FieldKind::Set => match source {
                    Json::Array(_) => source.clone(),
                    Json::Null => json!([]),
                    single => json!([single]),
                },
                FieldKind::Number => match source.as_f64() {
                    Some(number) if number.fract() == 0.0 && number.abs() < 9e15 => json!(number as i64),
                    _ => source.clone(),
                },
                _ => source.clone(),
            };
            row.insert(name.to_string(), value);
        }
        row
    }
}

/// A backend over one Elasticsearch or OpenSearch index.
pub struct ElasticBackend {
    collection: Collection,
    client: ElasticClient,
    index: String,
    options: ElasticOptions,
}

impl ElasticBackend {
    /// The index to search. Defaults to the collection's name.
    pub fn index(mut self, index: impl Into<String>) -> ElasticBackend {
        self.index = index.into();
        self
    }

    pub fn client(&self) -> &ElasticClient {
        &self.client
    }

    fn search_index(&self, body: &Json) -> Result<Json, Error> {
        self.client.request("POST", &format!("{}/_search", self.index), Some(body)).map_err(Error::Backend)
    }
}

impl Backend for ElasticBackend {
    fn search(&self, query: &Query, page: &PageRequest) -> Result<Page, Error> {
        let collection = &self.collection;
        let plan = plan(collection, query)?;
        let compiler = Compiler { collection, options: &self.options };
        let id = compiler.name(collection.id());
        let mut matching = compiler.filter(&plan.filter)?;
        let mut groups = None;

        if let Some(rollup) = &plan.rollup {
            // First find the row that stands for each group, by preference. The
            // search proper then pages over just those rows.
            let cap = self.options.max_groups;
            let body = json!({
                "size": 0,
                "query": matching,
                "aggs": { "groups": {
                    "terms": { "field": compiler.name(&rollup.by), "size": cap + 1 },
                    "aggs": { "pick": { "top_hits": {
                        "size": 1,
                        "sort": compiler.sort(&rollup.prefer),
                        "_source": [&id],
                    } } },
                } },
            });
            let answer = self.search_index(&body)?;
            let buckets = answer["aggregations"]["groups"]["buckets"].as_array().cloned().unwrap_or_default();
            if buckets.len() > cap {
                let message =
                    format!("This search makes more than {cap} groups, which is too many to roll up");
                return Err(Error::Query(QueryError::one("too_many_groups", message)));
            }
            let picked: Vec<Json> = buckets
                .iter()
                .map(|bucket| bucket["pick"]["hits"]["hits"][0]["_source"][&id].clone())
                .collect();
            groups = Some(picked.len() as u64);
            matching = json!({ "terms": { &id: picked } });
        }

        let dates: Vec<Json> = collection
            .fields()
            .filter(|(_, field)| field.kind == FieldKind::Date)
            .filter_map(|(name, _)| match compiler.date_of(name) {
                DateStorage::Instant => Some(json!({ "field": compiler.name(name), "format": ISO_MICROS })),
                DateStorage::Day => Some(json!({ "field": compiler.name(name), "format": "yyyy-MM-dd" })),
                DateStorage::Text => None,
            })
            .collect();
        let mut body = json!({
            "query": matching,
            "sort": compiler.sort(&plan.sort),
            // One row more than asked for shows whether another page follows.
            "size": page.limit.saturating_add(1),
            "from": page.offset,
            "docvalue_fields": dates,
            // The count is taken before the cursor's position is applied.
            "aggs": { "total": { "value_count": { "field": &id } } },
        });
        if let Some(token) = &page.after {
            let values = decode_cursor(collection, &plan.sort, token)?;
            body["post_filter"] = compiler.after(&plan.sort, &values);
        }

        let answer = self.search_index(&body)?;
        let hits = answer["hits"]["hits"].as_array().cloned().unwrap_or_default();
        let counted = answer["aggregations"]["total"]["value"].as_f64().unwrap_or(0.0) as u64;
        let has_more = hits.len() > page.limit;
        let rows: Vec<Row> = hits.iter().take(page.limit).map(|hit| compiler.row(hit)).collect();
        let next = match rows.last() {
            Some(last) if has_more => Some(encode_cursor(&plan.sort, last)),
            _ => None,
        };
        Ok(Page { rows, total: groups.unwrap_or(counted), has_more, next })
    }
}

/// A backend over the index named after the collection.
pub fn elastic_backend(
    collection: &Collection,
    client: ElasticClient,
    options: ElasticOptions,
) -> Result<ElasticBackend, DefinitionError> {
    options.check(collection)?;
    Ok(ElasticBackend {
        collection: collection.clone(),
        client,
        index: collection.name().to_string(),
        options,
    })
}
