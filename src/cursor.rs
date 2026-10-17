//! Cursors: a position in a sorted result.
//!
//! A cursor holds the sort key values of the last row of a page. The next page is the
//! rows that sort strictly after it, so rows added or removed in between never shift
//! later pages the way an offset does.
//!
//! The token is opaque to clients but not secret. Forging one only chooses where a page
//! starts, and its values are checked here and then sent as parameters.

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use serde_json::Value as Json;

use crate::ast::QueryError;
use crate::backend::Row;
use crate::collection::Collection;
use crate::plan::SortKey;
use crate::schema::FieldKind;

/// Identifies the order a cursor was made for, so it cannot be replayed against another.
fn signature(sort: &[SortKey]) -> String {
    let key = |key: &SortKey| {
        let direction = if key.descending { "desc" } else { "asc" };
        let nulls = if key.nulls_first { "first" } else { "last" };
        format!("{} {direction} {nulls}", key.field)
    };
    sort.iter().map(key).collect::<Vec<_>>().join(",")
}

/// The cursor for the position just after `row`.
pub fn encode_cursor(sort: &[SortKey], row: &Row) -> String {
    let mut parts = vec![Json::String(signature(sort))];
    parts.extend(sort.iter().map(|key| row.get(&key.field).cloned().unwrap_or(Json::Null)));
    URL_SAFE_NO_PAD.encode(Json::Array(parts).to_string())
}

/// The sort key values a cursor holds, one per key of `sort`. Fails if it does not fit.
pub fn decode_cursor(
    collection: &Collection,
    sort: &[SortKey],
    token: &str,
) -> Result<Vec<Json>, QueryError> {
    let invalid =
        || QueryError::one("invalid_cursor", "The cursor is malformed or was issued for a different order");
    let bytes = URL_SAFE_NO_PAD.decode(token).map_err(|_| invalid())?;
    let Ok(Json::Array(mut parts)) = serde_json::from_slice::<Json>(&bytes) else {
        return Err(invalid());
    };
    if parts.len() != sort.len() + 1 || parts[0] != Json::String(signature(sort)) {
        return Err(invalid());
    }
    let values = parts.split_off(1);

    for (key, value) in sort.iter().zip(&values) {
        let field = collection.field(&key.field);
        let fits = match (value, field.kind) {
            (Json::Null, _) => field.nullable,
            (Json::Number(number), FieldKind::Number) => number.as_f64().is_some_and(f64::is_finite),
            (Json::String(member), FieldKind::Ordinal) => field.rank(member).is_some(),
            (Json::String(_), FieldKind::Text | FieldKind::Keyword | FieldKind::Date) => true,
            _ => false,
        };
        if !fits {
            return Err(invalid());
        }
    }
    Ok(values)
}
