//! Compiles queries to SQL for any database, given its [`Dialect`].
//!
//! Values only ever travel as parameters. The SQL text is built from the collection's
//! declaration alone.

use serde_json::Value as Json;

use super::{Backend, Error, Page, PageRequest, Row};
use crate::ast::{Node, Query, QueryError, Value};
use crate::collection::Collection;
use crate::cursor::{decode_cursor, encode_cursor};
use crate::plan::{SortKey, plan};
use crate::schema::{FieldKind, Op};
use crate::semantics::{CompareOp, Effective, date_bounds, effective_op, ordinal_matches};

/// A column as a dialect sees it: the field it stores, and its qualified, quoted name.
#[derive(Clone, Debug)]
pub struct Column<'a> {
    pub field: &'a str,
    pub kind: FieldKind,
    pub sql: String,
}

/// What differs between SQL databases. Everything else the compiler emits is the common
/// subset: comparisons, `LIKE ... ESCAPE`, `CASE`, `COUNT(DISTINCT)` and `ROW_NUMBER()`.
///
/// Methods take the column they apply to, so a dialect can store fields of one kind in
/// different ways.
pub trait Dialect: Send + Sync {
    /// Quotes an identifier.
    fn ident(&self, name: &str) -> String;

    /// The placeholder for the parameter at a 1-based position. The parameter is given
    /// because some databases write its type into the placeholder.
    fn placeholder(&self, position: usize, param: &Param) -> String;

    /// A lower-cased text expression.
    fn lower(&self, expression: &str) -> String {
        format!("LOWER({expression})")
    }

    /// A case-insensitive test of whether a text column contains a parameter, for a
    /// database that has one. The parameter is the text, lower-cased. Without it the
    /// compiler falls back on `LIKE`.
    fn contains(&self, _column: &Column, _placeholder: &str) -> Option<String> {
        None
    }

    fn limit(&self, limit: &str, offset: &str) -> String {
        format!("LIMIT {limit} OFFSET {offset}")
    }

    /// Whether `limit` writes the offset before the limit. Parameters are bound in the
    /// order their placeholders appear.
    fn offset_first(&self) -> bool {
        false
    }

    /// Whether a character must be escaped to be matched literally by `LIKE`.
    fn like_special(&self, ch: char) -> bool {
        matches!(ch, '!' | '%' | '_')
    }

    /// Whether the database can match regular expressions at all.
    fn supports_regex(&self) -> bool {
        true
    }

    /// A case-insensitive regular expression match of a column against a parameter.
    fn regex(&self, column: &Column, placeholder: &str) -> String;

    /// The SQL for a number parameter, with any cast its value needs.
    fn number(&self, _column: &Column, placeholder: &str, _value: f64) -> String {
        placeholder.to_string()
    }

    /// The SQL and the bound text for a date parameter, given as an ISO date or timestamp.
    fn date(&self, _column: &Column, placeholder: &str, value: &str) -> (String, String) {
        (placeholder.to_string(), value.to_string())
    }

    /// The expression for a column when it is compared with the column of another field.
    fn operand(&self, column: &Column, _other: &Column) -> String {
        column.sql.clone()
    }

    /// Whether a set column contains every member of a set parameter.
    fn set_has_all(&self, column: &Column, placeholder: &str) -> String;

    /// Whether every member of a set column is in a set parameter.
    fn set_within(&self, column: &Column, placeholder: &str) -> String;

    /// The number of members in a set column.
    fn set_count(&self, column: &Column) -> String;

    /// The text to bind for a set parameter.
    fn encode_set(&self, column: &Column, members: &[String]) -> String;

    /// The expression that selects a column. A set must come back as the text of a JSON
    /// array of strings, and a date as the string a row holds.
    fn select(&self, column: &Column) -> String {
        column.sql.clone()
    }
}

/// A value bound to a placeholder.
#[derive(Clone, Debug, PartialEq)]
pub enum Param {
    Text(String),
    Int(i64),
    Real(f64),
}

impl Param {
    fn number(value: f64) -> Param {
        if value.fract() == 0.0 && value.abs() < 9e15 { Param::Int(value as i64) } else { Param::Real(value) }
    }
}

/// A value read from a result column.
#[derive(Clone, Debug, PartialEq)]
pub enum Cell {
    Null,
    Int(i64),
    Real(f64),
    Text(String),
}

/// Runs statements against a database.
pub trait SqlRunner: Send + Sync {
    /// Runs one statement and returns its rows, each a cell per selected column.
    fn run(&self, sql: &str, params: &[Param]) -> Result<Vec<Vec<Cell>>, String>;
}

#[derive(Clone, Debug, PartialEq)]
pub struct Statement {
    pub sql: String,
    pub params: Vec<Param>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Compiled {
    /// Selects one column per field of the collection, in declaration order. It asks for
    /// one row more than the page's limit, which is how the caller learns whether
    /// another page follows.
    pub page: Statement,
    /// Selects the number of rows the query matches, as its only column.
    pub count: Statement,
    /// The order the rows of `page` come back in.
    pub sort: Vec<SortKey>,
}

/// Stands for the next parameter in a fragment's text.
const MARK: char = '\0';
const MARK_STR: &str = "\0";

/// SQL text with the parameters for its marks, in order.
#[derive(Clone, Debug, Default)]
struct Frag {
    text: String,
    params: Vec<Param>,
}

impl Frag {
    fn new() -> Frag {
        Frag::default()
    }

    fn sql(mut self, text: &str) -> Frag {
        self.text.push_str(text);
        self
    }

    /// Appends a mark and binds a parameter to it.
    fn param(mut self, param: Param) -> Frag {
        self.text.push(MARK);
        self.params.push(param);
        self
    }

    /// Binds a parameter to a mark that a dialect has already written into the text.
    fn bind(mut self, param: Param) -> Frag {
        self.params.push(param);
        self
    }

    fn frag(mut self, other: Frag) -> Frag {
        self.text.push_str(&other.text);
        self.params.extend(other.params);
        self
    }

    fn join(parts: Vec<Frag>, separator: &str) -> Frag {
        let mut joined = Frag::new();
        for (index, part) in parts.into_iter().enumerate() {
            if index > 0 {
                joined.text.push_str(separator);
            }
            joined = joined.frag(part);
        }
        joined
    }

    fn finish(self, dialect: &dyn Dialect) -> Statement {
        let mut sql = String::with_capacity(self.text.len() + self.params.len() * 2);
        let mut position = 0;
        for ch in self.text.chars() {
            if ch == MARK {
                position += 1;
                let param = self.params.get(position - 1).expect("every placeholder has a parameter");
                sql.push_str(&dialect.placeholder(position, param));
            } else {
                sql.push(ch);
            }
        }
        debug_assert_eq!(position, self.params.len(), "every parameter has a placeholder");
        Statement { sql, params: self.params }
    }
}

struct Compiler<'a> {
    collection: &'a Collection,
    dialect: &'a dyn Dialect,
}

/// Compiles a query into a page statement and a count statement.
pub fn compile_sql(
    collection: &Collection,
    query: &Query,
    page: &PageRequest,
    dialect: &dyn Dialect,
    table: &str,
) -> Result<Compiled, QueryError> {
    let plan = plan(collection, query)?;
    if !dialect.supports_regex() && has_regex(collection, &plan.filter) {
        return Err(QueryError::one(
            "unsupported_regex",
            "Regular expressions are not available for this collection",
        ));
    }
    let compiler = Compiler { collection, dialect };
    let d = dialect;

    let source =
        Frag::new().sql(&format!(" FROM {} AS t WHERE ", d.ident(table))).frag(compiler.filter(&plan.filter));
    let columns: Vec<String> = collection
        .fields()
        .map(|(name, _)| format!("{} AS {}", d.select(&compiler.column(name)), d.ident(name)))
        .collect();
    let columns = columns.join(", ");

    let mut tail = Frag::new();
    if let Some(token) = &page.after {
        let values = decode_cursor(collection, &plan.sort, token)?;
        tail = tail.sql(" AND ").frag(compiler.after(&plan.sort, &values));
    }
    let limit = i64::try_from(page.limit).unwrap_or(i64::MAX).saturating_add(1);
    let offset = i64::try_from(page.offset).unwrap_or(i64::MAX);
    tail = tail
        .sql(" ORDER BY ")
        .frag(compiler.order_by(&plan.sort))
        .sql(&format!(" {}", d.limit(MARK_STR, MARK_STR)));
    let (first, second) = if d.offset_first() { (offset, limit) } else { (limit, offset) };
    tail = tail.bind(Param::Int(first)).bind(Param::Int(second));

    let count_of = |what: &str| {
        Frag::new().sql(&format!("SELECT {what} AS {}", d.ident("n"))).frag(source.clone()).finish(d)
    };
    let Some(rollup) = &plan.rollup else {
        let page = Frag::new().sql(&format!("SELECT {columns}")).frag(source.clone()).frag(tail);
        return Ok(Compiled { page: page.finish(d), count: count_of("COUNT(*)"), sort: plan.sort });
    };

    // Rank the rows of each group by preference and keep the first. The position of the
    // page is applied outside, once each group has settled on the row that stands for it.
    let group = compiler.column(&rollup.by).sql;
    let rank = d.ident("__rank");
    let page = Frag::new()
        .sql(&format!("SELECT {columns} FROM (SELECT t.*, ROW_NUMBER() OVER (PARTITION BY {group} ORDER BY "))
        .frag(compiler.order_by(&rollup.prefer))
        .sql(&format!(") AS {rank}"))
        .frag(source.clone())
        .sql(&format!(") AS t WHERE t.{rank} = 1"))
        .frag(tail);
    Ok(Compiled {
        page: page.finish(d),
        count: count_of(&format!("COUNT(DISTINCT {group})")),
        sort: plan.sort,
    })
}

/// Whether a filter matches by regular expression anywhere, predicates included.
fn has_regex(collection: &Collection, node: &Node) -> bool {
    match node {
        Node::And(children) | Node::Or(children) => children.iter().any(|child| has_regex(collection, child)),
        Node::Not(child) => has_regex(collection, child),
        Node::Is(name) => collection.predicate(name).is_some_and(|(_, filter)| has_regex(collection, filter)),
        Node::Cmp { value, .. } => matches!(value, Value::Regex(_)),
        Node::All | Node::Has(_) => false,
    }
}

impl<'a> Compiler<'a> {
    /// Every column is qualified, so it can never be mistaken for a selected alias: a
    /// field may be selected through an expression under the very name of its column.
    fn column(&self, field: &'a str) -> Column<'a> {
        let declared = self.collection.field(field);
        let name = declared.column.as_deref().unwrap_or(field);
        Column { field, kind: declared.kind, sql: format!("t.{}", self.dialect.ident(name)) }
    }

    fn filter(&self, node: &'a Node) -> Frag {
        match node {
            Node::All => Frag::new().sql("1 = 1"),
            Node::And(children) | Node::Or(children) => {
                let separator = if matches!(node, Node::And(_)) { " AND " } else { " OR " };
                let parts = children.iter().map(|child| self.filter(child)).collect();
                Frag::new().sql("(").frag(Frag::join(parts, separator)).sql(")")
            }
            Node::Not(child) => Frag::new().sql("NOT (").frag(self.filter(child)).sql(")"),
            Node::Is(name) => {
                let (_, filter) = self.collection.predicate(name).expect("a resolved predicate exists");
                self.filter(filter)
            }
            Node::Has(field) => {
                let column = self.column(field);
                Frag::new().sql(&match column.kind {
                    FieldKind::Set => format!("{} > 0", self.dialect.set_count(&column)),
                    _ => format!("{} IS NOT NULL", column.sql),
                })
            }
            Node::Cmp { field, op, value } => self.comparison(field, *op, value),
        }
    }

    /// A comparison that is never SQL NULL: a null cell makes it false. That keeps `NOT`
    /// a true complement, as it is in the in-memory backend.
    fn comparison(&self, name: &'a str, op: Op, value: &'a Value) -> Frag {
        let d = self.dialect;
        let field = self.collection.field(name);
        let column = self.column(name);
        let col = column.sql.as_str();
        let op = match effective_op(field, op) {
            Effective::Ne => {
                return Frag::new().sql("NOT (").frag(self.comparison(name, Op::Eq, value)).sql(")");
            }
            Effective::Contains => None,
            Effective::Compare(op) => Some(op),
        };
        // Every arm below with a comparison operator is only reached with one.
        let compare = op.unwrap_or(CompareOp::Eq);
        let cmp = compare.as_str();

        let present = |test: Frag, other: Option<&Column>| {
            let mut guards = Vec::new();
            if field.nullable {
                guards.push(format!("{col} IS NOT NULL"));
            }
            if let Some(other) = other.filter(|other| self.collection.field(other.field).nullable) {
                guards.push(format!("{} IS NOT NULL", other.sql));
            }
            if guards.is_empty() {
                return test;
            }
            Frag::new().sql(&format!("({} AND ", guards.join(" AND "))).frag(test).sql(")")
        };

        match value {
            Value::Text(text) => {
                let text = text.to_lowercase();
                let lowered = d.lower(col);
                let test = if let Some(contains) = d.contains(&column, MARK_STR).filter(|_| op.is_none()) {
                    Frag::new().sql(&contains).bind(Param::Text(text))
                } else if op.is_none() {
                    let mut pattern = String::from("%");
                    for ch in text.chars() {
                        if d.like_special(ch) {
                            pattern.push('!');
                        }
                        pattern.push(ch);
                    }
                    pattern.push('%');
                    Frag::new()
                        .sql(&format!("{lowered} LIKE "))
                        .param(Param::Text(pattern))
                        .sql(" ESCAPE '!'")
                } else {
                    Frag::new().sql(&format!("{lowered} = ")).param(Param::Text(text))
                };
                present(test, None)
            }
            Value::Regex(source) => {
                let test = Frag::new().sql(&d.regex(&column, MARK_STR)).bind(Param::Text(source.clone()));
                present(test, None)
            }
            Value::Keyword(wanted) => {
                present(Frag::new().sql(&format!("{col} = ")).param(Param::Text(wanted.clone())), None)
            }
            Value::Number(wanted) => {
                let sent = d.number(&column, MARK_STR, *wanted);
                present(Frag::new().sql(&format!("{col} {cmp} {sent}")).bind(Param::number(*wanted)), None)
            }
            Value::Ordinal(wanted) => {
                let matches = ordinal_matches(field, compare, wanted);
                if matches.is_empty() {
                    return Frag::new().sql("1 = 0");
                }
                let members = matches
                    .into_iter()
                    .map(|member| Frag::new().param(Param::Text(member.to_string())))
                    .collect();
                let test = Frag::new().sql(&format!("{col} IN (")).frag(Frag::join(members, ", ")).sql(")");
                present(test, None)
            }
            Value::Count(wanted) => Frag::new()
                .sql(&format!("{} {cmp} ", d.set_count(&column)))
                .param(Param::Int(i64::try_from(*wanted).unwrap_or(i64::MAX))),
            Value::Set(wanted) => {
                let members = || Param::Text(d.encode_set(&column, wanted));
                let has_all = || Frag::new().sql(&d.set_has_all(&column, MARK_STR)).bind(members());
                let within = || Frag::new().sql(&d.set_within(&column, MARK_STR)).bind(members());
                let both = |first: Frag, joiner: &str, second: Frag| {
                    Frag::new().sql("(").frag(first).sql(joiner).frag(second).sql(")")
                };
                match compare {
                    CompareOp::Ge => has_all(),
                    CompareOp::Le => within(),
                    CompareOp::Eq => both(has_all(), " AND ", within()),
                    CompareOp::Gt => both(has_all(), " AND NOT ", within()),
                    CompareOp::Lt => both(within(), " AND NOT ", has_all()),
                }
            }
            Value::Date { start, end, .. } => {
                let bounds = date_bounds(compare, start, end)
                    .into_iter()
                    .map(|(op, bound)| {
                        let (sent, bound) = d.date(&column, MARK_STR, bound);
                        Frag::new().sql(&format!("{col} {} {sent}", op.as_str())).bind(Param::Text(bound))
                    })
                    .collect();
                present(Frag::join(bounds, " AND "), None)
            }
            Value::Ref(other) => {
                let other = self.column(other);
                let test = format!("{} {cmp} {}", d.operand(&column, &other), d.operand(&other, &column));
                present(Frag::new().sql(&test), Some(&other))
            }
        }
    }

    fn order_by(&self, keys: &'a [SortKey]) -> Frag {
        let mut parts = Vec::new();
        for key in keys {
            let field = self.collection.field(&key.field);
            let col = self.column(&key.field).sql;
            if field.nullable {
                let (null, value) = if key.nulls_first { (0, 1) } else { (1, 0) };
                parts.push(Frag::new().sql(&format!("CASE WHEN {col} IS NULL THEN {null} ELSE {value} END")));
            }
            let direction = if key.descending { "DESC" } else { "ASC" };
            if field.kind == FieldKind::Ordinal {
                let mut ranked = Frag::new().sql(&format!("CASE {col}"));
                for (rank, member) in field.values.iter().enumerate() {
                    ranked =
                        ranked.sql(" WHEN ").param(Param::Text(member.clone())).sql(&format!(" THEN {rank}"));
                }
                parts.push(ranked.sql(&format!(" END {direction}")));
            } else {
                parts.push(Frag::new().sql(&format!("{col} {direction}")));
            }
        }
        Frag::join(parts, ", ")
    }

    /// The rows that sort strictly after a cursor: later on the first key, or level on
    /// it and after the cursor on the keys that remain.
    fn after(&self, keys: &'a [SortKey], values: &[Json]) -> Frag {
        let d = self.dialect;
        let (key, rest) = keys.split_first().expect("a sort has at least the id");
        let value = &values[0];
        let field = self.collection.field(&key.field);
        let column = self.column(&key.field);
        let col = column.sql.as_str();

        // The cursor's value, as a parameter of the column's type.
        let sent = || match (field.kind, value) {
            (FieldKind::Number, Json::Number(number)) => {
                let number = number.as_f64().unwrap_or_default();
                Frag::new().sql(&d.number(&column, MARK_STR, number)).bind(Param::number(number))
            }
            (FieldKind::Date, Json::String(date)) => {
                let (sql, bound) = d.date(&column, MARK_STR, date);
                Frag::new().sql(&sql).bind(Param::Text(bound))
            }
            _ => Frag::new().param(Param::Text(value.as_str().unwrap_or_default().to_string())),
        };

        // Nulls sit at one end whatever the direction, so they are handled apart from values.
        let mut later: Option<Frag> = None;
        if value.is_null() {
            if key.nulls_first {
                later = Some(Frag::new().sql(&format!("{col} IS NOT NULL")));
            }
        } else {
            if field.kind == FieldKind::Ordinal {
                let rank = value.as_str().and_then(|member| field.rank(member)).unwrap_or(0);
                let beyond: Vec<Frag> = field
                    .values
                    .iter()
                    .enumerate()
                    .filter(|(index, _)| if key.descending { *index < rank } else { *index > rank })
                    .map(|(_, member)| Frag::new().param(Param::Text(member.clone())))
                    .collect();
                if !beyond.is_empty() {
                    later =
                        Some(Frag::new().sql(&format!("{col} IN (")).frag(Frag::join(beyond, ", ")).sql(")"));
                }
            } else {
                let cmp = if key.descending { "<" } else { ">" };
                later = Some(Frag::new().sql(&format!("{col} {cmp} ")).frag(sent()));
            }
            if field.nullable && !key.nulls_first {
                later = Some(match later {
                    Some(later) => Frag::new().sql("(").frag(later).sql(&format!(" OR {col} IS NULL)")),
                    None => Frag::new().sql(&format!("{col} IS NULL")),
                });
            }
        }
        if rest.is_empty() {
            return later.unwrap_or_else(|| Frag::new().sql("1 = 0"));
        }

        let level = if value.is_null() {
            Frag::new().sql(&format!("{col} IS NULL"))
        } else {
            Frag::new().sql(&format!("{col} = ")).frag(sent())
        };
        let then =
            Frag::new().sql("(").frag(level).sql(" AND ").frag(self.after(rest, &values[1..])).sql(")");
        match later {
            Some(later) => Frag::new().sql("(").frag(later).sql(" OR ").frag(then).sql(")"),
            None => then,
        }
    }
}

/// Converts a cell into the value a row holds for a field of the given kind.
fn decode(kind: FieldKind, cell: Cell) -> Result<Json, String> {
    let number = |value: f64| {
        if value.fract() == 0.0 && value.abs() < 9e15 {
            Json::from(value as i64)
        } else {
            serde_json::Number::from_f64(value).map_or(Json::Null, Json::Number)
        }
    };
    Ok(match (kind, cell) {
        (_, Cell::Null) => Json::Null,
        (FieldKind::Number, Cell::Int(value)) => Json::from(value),
        (FieldKind::Number, Cell::Real(value)) => number(value),
        (FieldKind::Number, Cell::Text(text)) => {
            number(text.trim().parse().map_err(|_| format!("\"{text}\" is not a number"))?)
        }
        (FieldKind::Set, Cell::Text(text)) => match serde_json::from_str(&text) {
            Ok(members @ Json::Array(_)) => members,
            _ => return Err(format!("\"{text}\" is not a JSON array")),
        },
        (FieldKind::Set, cell) => return Err(format!("{cell:?} is not a set")),
        (_, Cell::Text(text)) => Json::String(text),
        (_, Cell::Int(value)) => Json::String(value.to_string()),
        (_, Cell::Real(value)) => Json::String(value.to_string()),
    })
}

/// A backend over any SQL database, given its dialect and a way to run statements.
pub struct SqlBackend<D, R> {
    collection: Collection,
    dialect: D,
    runner: R,
    table: String,
}

impl<D: Dialect, R: SqlRunner> SqlBackend<D, R> {
    /// A backend over the table named after the collection.
    pub fn new(collection: &Collection, dialect: D, runner: R) -> SqlBackend<D, R> {
        let table = collection.name().to_string();
        SqlBackend { collection: collection.clone(), dialect, runner, table }
    }

    pub fn table(mut self, table: impl Into<String>) -> SqlBackend<D, R> {
        self.table = table.into();
        self
    }

    pub fn runner(&self) -> &R {
        &self.runner
    }
}

impl<D: Dialect, R: SqlRunner> Backend for SqlBackend<D, R> {
    fn search(&self, query: &Query, page: &PageRequest) -> Result<Page, Error> {
        let compiled = compile_sql(&self.collection, query, page, &self.dialect, &self.table)?;
        let run = |statement: &Statement| {
            self.runner.run(&statement.sql, &statement.params).map_err(Error::Backend)
        };

        let total = match run(&compiled.count)?.first().and_then(|row| row.first()) {
            Some(Cell::Int(total)) => u64::try_from(*total).unwrap_or(0),
            Some(Cell::Text(total)) => total.parse().unwrap_or(0),
            other => return Err(Error::Backend(format!("the count came back as {other:?}"))),
        };

        let mut found = run(&compiled.page)?;
        let has_more = found.len() > page.limit;
        found.truncate(page.limit);
        let mut rows = Vec::with_capacity(found.len());
        for cells in found {
            let mut row = Row::new();
            for ((name, field), cell) in self.collection.fields().zip(cells) {
                let value = decode(field.kind, cell)
                    .map_err(|why| Error::Backend(format!("field \"{name}\": {why}")))?;
                row.insert(name.to_string(), value);
            }
            rows.push(row);
        }
        let next = match rows.last() {
            Some(last) if has_more => Some(encode_cursor(&compiled.sort, last)),
            _ => None,
        };
        Ok(Page { rows, total, has_more, next })
    }
}
