//! A collection as a search endpoint, independent of any HTTP framework.
//!
//! Give [`respond`] the request's path and query string, and send back the status and
//! JSON body it returns. Routing and methods are the framework's concern.
//!
//! `?q=<query>` with optional `order`, `dir`, `unique`, `prefer` and `include` (comma
//! separated). A directive inside `q` overrides the same parameter.
//!
//! Results are walked by following `next_page`, which carries a `cursor`. `page` (from 1)
//! jumps to a position by counting rows instead, and cannot be combined with a cursor.

use serde_json::{Value as Json, json};

use crate::ast::{Issue, QueryError};
use crate::backend::{Backend, Error, PageRequest};
use crate::collection::Collection;
use crate::resolve::{Directives, ParseOptions, parse};

#[derive(Clone, Debug)]
pub struct HandlerOptions {
    /// Rows per page.
    pub page_size: usize,
    /// The longest query accepted, in characters.
    pub max_query_length: usize,
    /// Drop invalid terms with a warning instead of rejecting the request.
    pub lenient: bool,
    /// Prepended to the request's path in `next_page`, such as `https://api.example.com`.
    /// Left empty, `next_page` is relative to the host.
    pub base_url: String,
}

impl Default for HandlerOptions {
    fn default() -> HandlerOptions {
        HandlerOptions { page_size: 50, max_query_length: 1000, lenient: false, base_url: String::new() }
    }
}

/// A status and a JSON body: a list on success, an error object otherwise.
#[derive(Clone, Debug, PartialEq)]
pub struct Response {
    pub status: u16,
    pub body: Json,
}

fn issues_json(issues: &[Issue]) -> Json {
    issues
        .iter()
        .map(|issue| {
            let mut object = json!({ "code": issue.code, "message": issue.message });
            if let Some(span) = issue.span {
                object["span"] = json!({ "start": span.start, "end": span.end });
            }
            object
        })
        .collect()
}

fn failure(status: u16, code: &str, kind: &str, details: &str, issues: &[Issue]) -> Response {
    let body = json!({
        "object": "error",
        "status": status,
        "code": code,
        "type": kind,
        "details": details,
        "issues": issues_json(issues),
    });
    Response { status, body }
}

fn bad_query(error: &QueryError) -> Response {
    let kind = error.issues.first().map_or("bad_request", |issue| issue.code);
    failure(400, "bad_request", kind, &error.to_string(), &error.issues)
}

/// Answers a search request, given its target: the path with any query string, such as
/// `/tickets?q=is%3Aopen`.
pub fn respond(
    collection: &Collection,
    backend: &dyn Backend,
    target: &str,
    options: &HandlerOptions,
) -> Response {
    let (path, query_string) = target.split_once('?').unwrap_or((target, ""));
    let pairs: Vec<(String, String, &str)> = query_string
        .split('&')
        .filter(|pair| !pair.is_empty())
        .map(|pair| {
            let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
            (decode(key), decode(value), pair)
        })
        .collect();
    let param = |name: &str| pairs.iter().find(|(key, ..)| key == name).map(|(_, value, _)| value.clone());

    let input = param("q").unwrap_or_default();
    if input.chars().count() > options.max_query_length {
        let details = format!("The query is longer than {} characters", options.max_query_length);
        return failure(400, "bad_request", "query_too_long", &details, &[]);
    }
    let page = match param("page").map(|page| page.parse::<usize>()) {
        None => 1,
        Some(Ok(page)) if page >= 1 => page,
        Some(_) => {
            return failure(400, "bad_request", "invalid_page", "page must be a whole number from 1", &[]);
        }
    };
    let cursor = param("cursor");
    if cursor.is_some() && param("page").is_some() {
        return failure(400, "bad_request", "invalid_page", "Use either cursor or page, not both", &[]);
    }

    let parse_options = ParseOptions {
        lenient: options.lenient,
        now_ms: None,
        defaults: Directives {
            order: param("order"),
            direction: param("dir"),
            unique: param("unique"),
            prefer: param("prefer"),
            include: param("include")
                .map(|names| names.split(',').filter(|name| !name.is_empty()).map(str::to_string).collect())
                .unwrap_or_default(),
        },
    };
    let query = match parse(collection, &input, &parse_options) {
        Ok(query) => query,
        Err(error) => return bad_query(&error),
    };

    let request = PageRequest {
        limit: options.page_size,
        after: cursor,
        offset: (page - 1).saturating_mul(options.page_size),
    };
    let found = match backend.search(&query, &request) {
        Ok(found) => found,
        Err(Error::Query(error)) => return bad_query(&error),
        // What went wrong in the store is for the operator, not the caller.
        Err(Error::Backend(_)) => {
            return failure(500, "internal_error", "backend_failure", "The search could not be run", &[]);
        }
    };

    let next_page = found.next.as_ref().map(|next| {
        let mut kept: Vec<&str> =
            pairs.iter().filter(|(key, ..)| key != "page" && key != "cursor").map(|(.., raw)| *raw).collect();
        let cursor = format!("cursor={next}");
        kept.push(&cursor);
        format!("{}{path}?{}", options.base_url, kept.join("&"))
    });
    let body = json!({
        "object": "list",
        "data": found.rows,
        "has_more": found.has_more,
        "next_page": next_page,
        "total": found.total,
        "warnings": issues_json(&query.warnings),
    });
    Response { status: 200, body }
}

/// Decodes one component of a query string: `+` is a space and `%XX` a byte.
fn decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        let hex = |at: usize| bytes.get(at).and_then(|byte| (*byte as char).to_digit(16));
        match bytes[index] {
            b'+' => decoded.push(b' '),
            b'%' => match (hex(index + 1), hex(index + 2)) {
                (Some(high), Some(low)) => {
                    decoded.push((high * 16 + low) as u8);
                    index += 2;
                }
                _ => decoded.push(b'%'),
            },
            byte => decoded.push(byte),
        }
        index += 1;
    }
    String::from_utf8_lossy(&decoded).into_owned()
}
