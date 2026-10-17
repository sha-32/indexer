//! The resolved form of a query: a filter tree plus the directives that shape the list.

use std::fmt;

use crate::schema::Op;

/// Character offsets into the query string, end exclusive. A character is a Unicode
/// scalar value, not a byte.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Span {
    pub start: usize,
    pub end: usize,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Issue {
    /// A stable label such as `unknown_field`.
    pub code: &'static str,
    pub message: String,
    pub span: Option<Span>,
}

impl Issue {
    pub fn new(code: &'static str, message: impl Into<String>) -> Issue {
        Issue { code, message: message.into(), span: None }
    }
}

/// A query that could not be parsed or resolved against its collection.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct QueryError {
    pub issues: Vec<Issue>,
}

impl QueryError {
    pub fn one(code: &'static str, message: impl Into<String>) -> QueryError {
        QueryError { issues: vec![Issue::new(code, message)] }
    }
}

impl fmt::Display for QueryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let messages: Vec<&str> = self.issues.iter().map(|issue| issue.message.as_str()).collect();
        f.write_str(&messages.join("; "))
    }
}

impl std::error::Error for QueryError {}

/// What a field is compared against. Which variants a comparison may hold follows from
/// the kind of its field, and is enforced when the query is resolved.
#[derive(Clone, Debug, PartialEq)]
pub enum Value {
    Text(String),
    /// The source of a case-insensitive regular expression.
    Regex(String),
    Number(f64),
    Keyword(String),
    Ordinal(String),
    /// Members of a set field, in declaration order.
    Set(Vec<String>),
    /// A number compared against the size of a set.
    Count(u64),
    /// A date literal resolved to the half-open range `[start, end)` it covers. `text`
    /// is the literal as written.
    Date {
        start: String,
        end: String,
        text: String,
    },
    /// Another field of the same kind, as in `spent>estimate`.
    Ref(String),
}

/// A filter. Every field and predicate named in one exists in the collection it was
/// resolved against.
#[derive(Clone, Debug, PartialEq)]
pub enum Node {
    /// Matches every row.
    All,
    And(Vec<Node>),
    Or(Vec<Node>),
    Not(Box<Node>),
    Cmp {
        field: String,
        op: Op,
        value: Value,
    },
    /// The field is not null or, for a set, not empty.
    Has(String),
    /// A predicate declared on the collection.
    Is(String),
}

impl Node {
    /// Joins nodes with AND, flattening nested ANDs. No nodes means "match everything".
    pub fn and(nodes: impl IntoIterator<Item = Node>) -> Node {
        let mut children = Vec::new();
        for node in nodes {
            match node {
                Node::And(inner) => children.extend(inner),
                other => children.push(other),
            }
        }
        match children.len() {
            0 => Node::All,
            1 => children.remove(0),
            _ => Node::And(children),
        }
    }

    /// Joins nodes with OR, flattening nested ORs.
    pub fn or(nodes: impl IntoIterator<Item = Node>) -> Node {
        let mut children = Vec::new();
        for node in nodes {
            match node {
                Node::Or(inner) => children.extend(inner),
                other => children.push(other),
            }
        }
        match children.len() {
            0 => Node::All,
            1 => children.remove(0),
            _ => Node::Or(children),
        }
    }

    #[allow(clippy::should_implement_trait)]
    pub fn not(child: Node) -> Node {
        Node::Not(Box::new(child))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Direction {
    /// Each order's natural direction.
    #[default]
    Auto,
    Asc,
    Desc,
}

/// A filter plus the directives that shape the result list.
#[derive(Clone, Debug, PartialEq)]
pub struct Query {
    pub filter: Node,
    pub order: Option<String>,
    pub direction: Direction,
    pub unique: Option<String>,
    pub prefer: Option<String>,
    pub include: Vec<String>,
    /// Terms dropped in lenient mode. Always empty in strict mode.
    pub warnings: Vec<Issue>,
}

impl Query {
    /// A query for a filter built in code, with no directives.
    pub fn new(filter: Node) -> Query {
        Query {
            filter,
            order: None,
            direction: Direction::Auto,
            unique: None,
            prefer: None,
            include: Vec::new(),
            warnings: Vec::new(),
        }
    }
}
