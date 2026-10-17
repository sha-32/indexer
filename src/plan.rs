//! A query with its directives resolved to concrete fields. Backends execute this.

use crate::ast::{Direction, Node, Query, QueryError};
use crate::collection::Collection;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SortKey {
    pub field: String,
    pub descending: bool,
    /// Nulls sit at this end whatever the direction.
    pub nulls_first: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Rollup {
    /// Keep one row per value of this field.
    pub by: String,
    /// The row kept is the first one in this order.
    pub prefer: Vec<SortKey>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Plan {
    pub filter: Node,
    /// A total order: the id is always one of the keys, and last unless the order names it.
    pub sort: Vec<SortKey>,
    pub rollup: Option<Rollup>,
}

/// Plans a query. Fails if it names an order, rollup or preference the collection does
/// not have, which a query from [`crate::parse`] never does.
pub fn plan(collection: &Collection, query: &Query) -> Result<Plan, QueryError> {
    let unknown = |code: &'static str, what: &str, name: &str| {
        QueryError::one(code, format!("Unknown {what} \"{name}\""))
    };
    let id =
        |descending: bool| SortKey { field: collection.id().to_string(), descending, nulls_first: false };

    let hidden = collection
        .exclusions()
        .filter(|(name, _)| !query.include.iter().any(|included| included == name))
        .map(|(_, filter)| Node::not(filter.clone()));
    let filter = Node::and(std::iter::once(query.filter.clone()).chain(hidden));

    let order_name = query.order.as_deref().or(collection.default_order());
    let sort = match order_name {
        None => vec![id(query.direction == Direction::Desc)],
        Some(name) => {
            let (_, order) = collection.order(name).ok_or_else(|| unknown("unknown_order", "order", name))?;
            let descending = match query.direction {
                Direction::Auto => order.descending,
                direction => direction == Direction::Desc,
            };
            let mut keys: Vec<SortKey> = order
                .by
                .iter()
                .map(|field| SortKey { field: field.clone(), descending, nulls_first: order.nulls_first })
                .collect();
            if !keys.iter().any(|key| key.field == collection.id()) {
                keys.push(id(false));
            }
            keys
        }
    };

    let rollup = match &query.unique {
        None => None,
        Some(name) => {
            let (_, by) = collection.rollup(name).ok_or_else(|| unknown("unknown_rollup", "rollup", name))?;
            let mut prefer = Vec::new();
            if let Some(name) = &query.prefer {
                let (_, preference) = collection
                    .preference(name)
                    .ok_or_else(|| unknown("unknown_preference", "preference", name))?;
                prefer.push(SortKey {
                    field: preference.by.clone(),
                    descending: preference.descending,
                    nulls_first: false,
                });
            }
            prefer.push(id(false));
            Some(Rollup { by: by.to_string(), prefer })
        }
    };

    Ok(Plan { filter, sort, rollup })
}
