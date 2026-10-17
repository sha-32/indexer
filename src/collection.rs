//! A collection: the single declaration that parsing, validation and backends all follow.

use std::collections::HashMap;
use std::fmt;

use crate::ast::Node;
use crate::resolve::{ParseOptions, parse};
use crate::schema::{Field, FieldKind};

/// Words that have a fixed meaning before an operator and so cannot name a field.
pub const RESERVED: [&str; 12] =
    ["is", "not", "has", "order", "sort", "direction", "dir", "unique", "prefer", "include", "and", "or"];

/// A named sort. `order:<name>` selects it.
#[derive(Clone, Debug)]
pub struct Order {
    /// Fields to sort on, most significant first. The id is always the final tie-break.
    pub by: Vec<String>,
    /// Whether `direction:auto` sorts this order descending.
    pub descending: bool,
    /// Whether nulls come first. They sit at the same end whatever the direction.
    pub nulls_first: bool,
}

impl Order {
    /// An order on these fields, ascending by nature, nulls last.
    pub fn by<S: Into<String>>(fields: impl IntoIterator<Item = S>) -> Order {
        Order { by: fields.into_iter().map(Into::into).collect(), descending: false, nulls_first: false }
    }

    /// Make descending the natural direction.
    pub fn desc(mut self) -> Order {
        self.descending = true;
        self
    }

    pub fn nulls_first(mut self) -> Order {
        self.nulls_first = true;
        self
    }
}

/// A way to pick the row a rollup keeps, for `prefer:<name>`.
#[derive(Clone, Debug)]
pub struct Preference {
    pub by: String,
    pub descending: bool,
}

/// A collection declaration that contradicts itself.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DefinitionError(pub String);

impl fmt::Display for DefinitionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for DefinitionError {}

/// A validated collection. Create one with [`Collection::builder`].
#[derive(Clone, Debug)]
pub struct Collection {
    name: String,
    id: String,
    default: String,
    fields: Vec<(String, Field)>,
    /// Lower-cased field names and aliases, mapped to an index into `fields`.
    names: HashMap<String, usize>,
    predicates: Vec<(String, Node)>,
    orders: Vec<(String, Order)>,
    default_order: Option<String>,
    rollups: Vec<(String, String)>,
    preferences: Vec<(String, Preference)>,
    exclusions: Vec<(String, Node)>,
    /// Plain-word descriptions, keyed by the term they describe, such as `is:overdue`.
    descriptions: Vec<(String, String)>,
}

/// Finds an entry by name, ignoring case.
fn find<'a, T>(entries: &'a [(String, T)], name: &str) -> Option<&'a (String, T)> {
    entries.iter().find(|(key, _)| key.eq_ignore_ascii_case(name))
}

impl Collection {
    pub fn builder(name: impl Into<String>) -> CollectionBuilder {
        CollectionBuilder {
            name: name.into(),
            id: None,
            default: None,
            fields: Vec::new(),
            predicates: Vec::new(),
            orders: Vec::new(),
            default_order: None,
            rollups: Vec::new(),
            preferences: Vec::new(),
            exclusions: Vec::new(),
            descriptions: Vec::new(),
        }
    }

    /// Names the collection. SQL backends use it as the default table name.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The field that uniquely identifies a row.
    pub fn id(&self) -> &str {
        &self.id
    }

    /// The text field that bare words and `!exact` phrases match.
    pub fn default_field(&self) -> &str {
        &self.default
    }

    /// Every field with its name, in declaration order.
    pub fn fields(&self) -> impl Iterator<Item = (&str, &Field)> {
        self.fields.iter().map(|(name, field)| (name.as_str(), field))
    }

    /// The canonical name of the field with this name or alias, ignoring case.
    pub fn resolve(&self, name: &str) -> Option<&str> {
        let index = *self.names.get(&name.to_ascii_lowercase())?;
        Some(self.fields[index].0.as_str())
    }

    /// A field by canonical name.
    ///
    /// # Panics
    /// If no field has that name. Names taken from a resolved [`Node`] or a
    /// [`crate::plan::Plan`] of this collection always exist.
    pub fn field(&self, name: &str) -> &Field {
        match self.names.get(&name.to_ascii_lowercase()) {
            Some(&index) if self.fields[index].0 == name => &self.fields[index].1,
            _ => panic!("collection \"{}\" has no field \"{name}\"", self.name),
        }
    }

    /// The canonical name and filter of a predicate, ignoring case.
    pub fn predicate(&self, name: &str) -> Option<(&str, &Node)> {
        find(&self.predicates, name).map(|(key, node)| (key.as_str(), node))
    }

    pub fn order(&self, name: &str) -> Option<(&str, &Order)> {
        find(&self.orders, name).map(|(key, order)| (key.as_str(), order))
    }

    /// The order used when a query names none. Without one, rows sort by id.
    pub fn default_order(&self) -> Option<&str> {
        self.default_order.as_deref()
    }

    /// The canonical name of a rollup and the field it groups by.
    pub fn rollup(&self, name: &str) -> Option<(&str, &str)> {
        find(&self.rollups, name).map(|(key, by)| (key.as_str(), by.as_str()))
    }

    pub fn preference(&self, name: &str) -> Option<(&str, &Preference)> {
        find(&self.preferences, name).map(|(key, preference)| (key.as_str(), preference))
    }

    pub fn exclusion(&self, name: &str) -> Option<(&str, &Node)> {
        find(&self.exclusions, name).map(|(key, node)| (key.as_str(), node))
    }

    /// Every predicate with its filter, in declaration order.
    pub fn predicates(&self) -> impl Iterator<Item = (&str, &Node)> {
        self.predicates.iter().map(|(name, node)| (name.as_str(), node))
    }

    /// Every order, in declaration order.
    pub fn orders(&self) -> impl Iterator<Item = (&str, &Order)> {
        self.orders.iter().map(|(name, order)| (name.as_str(), order))
    }

    /// Every rollup with the field it groups by, in declaration order.
    pub fn rollups(&self) -> impl Iterator<Item = (&str, &str)> {
        self.rollups.iter().map(|(name, by)| (name.as_str(), by.as_str()))
    }

    /// Every preference, in declaration order.
    pub fn preferences(&self) -> impl Iterator<Item = (&str, &Preference)> {
        self.preferences.iter().map(|(name, preference)| (name.as_str(), preference))
    }

    /// The description given to a term such as `is:overdue` or `order:created`.
    pub fn description(&self, term: &str) -> Option<&str> {
        find(&self.descriptions, term).map(|(_, text)| text.as_str())
    }

    /// Every exclusion: a filter whose rows are hidden unless `include:<name>` is given.
    pub fn exclusions(&self) -> impl Iterator<Item = (&str, &Node)> {
        self.exclusions.iter().map(|(name, node)| (name.as_str(), node))
    }
}

/// Collects a declaration. Nothing is checked until [`CollectionBuilder::build`].
pub struct CollectionBuilder {
    name: String,
    id: Option<String>,
    default: Option<String>,
    fields: Vec<(String, Field)>,
    predicates: Vec<(String, String)>,
    orders: Vec<(String, Order)>,
    default_order: Option<String>,
    rollups: Vec<(String, String)>,
    preferences: Vec<(String, Preference)>,
    exclusions: Vec<(String, String)>,
    descriptions: Vec<(String, String)>,
}

impl CollectionBuilder {
    pub fn field(mut self, name: impl Into<String>, field: Field) -> Self {
        self.fields.push((name.into(), field));
        self
    }

    /// The field that uniquely identifies a row.
    pub fn id(mut self, field: impl Into<String>) -> Self {
        self.id = Some(field.into());
        self
    }

    /// The text field that bare words and `!exact` phrases match.
    pub fn default_field(mut self, field: impl Into<String>) -> Self {
        self.default = Some(field.into());
        self
    }

    /// A named filter for `is:<name>` and `not:<name>`, written as a query. It may use
    /// predicates declared before it.
    pub fn predicate(mut self, name: impl Into<String>, query: impl Into<String>) -> Self {
        self.predicates.push((name.into(), query.into()));
        self
    }

    pub fn order(mut self, name: impl Into<String>, order: Order) -> Self {
        self.orders.push((name.into(), order));
        self
    }

    pub fn default_order(mut self, name: impl Into<String>) -> Self {
        self.default_order = Some(name.into());
        self
    }

    /// A way to collapse rows for `unique:<name>`: one row is kept per value of `by`.
    pub fn rollup(mut self, name: impl Into<String>, by: impl Into<String>) -> Self {
        self.rollups.push((name.into(), by.into()));
        self
    }

    /// A way to pick the row a rollup keeps, for `prefer:<name>`: the one with the
    /// lowest value of `by`, or the highest if `descending`.
    pub fn preference(mut self, name: impl Into<String>, by: impl Into<String>, descending: bool) -> Self {
        self.preferences.push((name.into(), Preference { by: by.into(), descending }));
        self
    }

    /// A filter, written as a query, whose rows are hidden unless `include:<name>` is given.
    pub fn exclusion(mut self, name: impl Into<String>, query: impl Into<String>) -> Self {
        self.exclusions.push((name.into(), query.into()));
        self
    }

    /// Say, in plain words, what a named term means. The term is written as it is in a
    /// query: `is:overdue`, `order:created`, `unique:story`, `prefer:newest` or
    /// `include:archived`. Shown in the syntax guide. Fields are described on the field.
    pub fn describe(mut self, term: impl Into<String>, description: impl Into<String>) -> Self {
        self.descriptions.push((term.into(), description.into()));
        self
    }

    /// Checks the declaration and returns the collection, so that mistakes surface at
    /// startup and not on the first query that happens to touch them.
    pub fn build(self) -> Result<Collection, DefinitionError> {
        let name = self.name.clone();
        let fail = |message: String| DefinitionError(format!("Collection \"{name}\": {message}"));

        let mut names = HashMap::new();
        for (index, (field_name, field)) in self.fields.iter().enumerate() {
            for key in std::iter::once(field_name).chain(&field.aliases) {
                let lower = key.to_ascii_lowercase();
                let mut chars = key.chars();
                let starts_well = chars.next().is_some_and(|ch| ch.is_ascii_alphabetic() || ch == '_');
                if !starts_well || !chars.all(|ch| ch.is_ascii_alphanumeric() || ch == '_') {
                    return Err(fail(format!("\"{key}\" is not a valid field name or alias")));
                }
                if RESERVED.contains(&lower.as_str()) {
                    return Err(fail(format!("\"{key}\" is a reserved word")));
                }
                if names.insert(lower, index).is_some() {
                    return Err(fail(format!("\"{key}\" is declared more than once")));
                }
            }
            if matches!(field.kind, FieldKind::Set | FieldKind::Ordinal) {
                let bad = |value: &&String| {
                    value.is_empty() || value.chars().any(|ch| ch.is_whitespace() || ",\"()".contains(ch))
                };
                if let Some(value) = field.values.iter().find(bad) {
                    return Err(fail(format!(
                        "value \"{value}\" of field \"{field_name}\" is empty or has punctuation"
                    )));
                }
                if field.values.is_empty() {
                    return Err(fail(format!("field \"{field_name}\" has no values")));
                }
            }
            for (nickname, members) in &field.nicknames {
                if let Some(member) = members.iter().find(|member| !field.values.contains(member)) {
                    return Err(fail(format!(
                        "nickname \"{nickname}\" of field \"{field_name}\" names unknown member \"{member}\""
                    )));
                }
            }
        }

        let fields = &self.fields;
        let sortable = |field_name: &str, what: &str, allow_null: bool| {
            let Some((_, field)) = fields.iter().find(|(key, _)| key == field_name) else {
                return Err(fail(format!("{what} refers to unknown field \"{field_name}\"")));
            };
            if !field.kind.sortable() {
                return Err(fail(format!("{what} cannot use set field \"{field_name}\"")));
            }
            if !allow_null && field.nullable {
                return Err(fail(format!("{what} cannot use nullable field \"{field_name}\"")));
            }
            Ok(())
        };

        let id = self.id.ok_or_else(|| fail("no id field was named".into()))?;
        sortable(&id, "id", false)?;
        let default = self.default.ok_or_else(|| fail("no default field was named".into()))?;
        if !fields.iter().any(|(key, field)| *key == default && field.kind == FieldKind::Text) {
            return Err(fail("the default field must be a text field".into()));
        }

        for (order_name, order) in &self.orders {
            if order.by.is_empty() {
                return Err(fail(format!("order \"{order_name}\" has no fields")));
            }
            for field_name in &order.by {
                sortable(field_name, &format!("order \"{order_name}\""), true)?;
            }
        }
        if let Some(order_name) = &self.default_order
            && !self.orders.iter().any(|(key, _)| key == order_name)
        {
            return Err(fail(format!("default order \"{order_name}\" is not a declared order")));
        }
        for (rollup_name, by) in &self.rollups {
            sortable(by, &format!("rollup \"{rollup_name}\""), false)?;
        }
        for (preference_name, preference) in &self.preferences {
            sortable(&preference.by, &format!("preference \"{preference_name}\""), true)?;
        }

        let mut collection = Collection {
            name: self.name,
            id,
            default,
            fields: self.fields,
            names,
            predicates: Vec::new(),
            orders: self.orders,
            default_order: self.default_order,
            rollups: self.rollups,
            preferences: self.preferences,
            exclusions: Vec::new(),
            descriptions: Vec::new(),
        };

        // Each predicate is resolved against the collection so far, so it may use
        // the ones before it.
        let filter = |collection: &Collection, what: &str, source: &str| {
            let invalid = |why: String| fail(format!("{what} \"{source}\" is invalid: {why}"));
            let query = parse(collection, source, &ParseOptions::default())
                .map_err(|error| invalid(error.to_string()))?;
            let shapes_list = query.order.is_some()
                || query.unique.is_some()
                || query.prefer.is_some()
                || !query.include.is_empty()
                || query.direction != crate::ast::Direction::Auto;
            if shapes_list {
                return Err(invalid("it may only filter".into()));
            }
            Ok(query.filter)
        };
        for (predicate_name, source) in self.predicates {
            let node = filter(&collection, &format!("predicate \"{predicate_name}\""), &source)?;
            collection.predicates.push((predicate_name, node));
        }
        for (exclusion_name, source) in self.exclusions {
            let node = filter(&collection, &format!("exclusion \"{exclusion_name}\""), &source)?;
            collection.exclusions.push((exclusion_name, node));
        }
        for (term, text) in self.descriptions {
            let (kind, name) = term.split_once(':').unwrap_or(("", &term));
            let canonical = match kind.to_ascii_lowercase().as_str() {
                "is" => collection.predicate(name).map(|(name, _)| format!("is:{name}")),
                "order" => collection.order(name).map(|(name, _)| format!("order:{name}")),
                "unique" => collection.rollup(name).map(|(name, _)| format!("unique:{name}")),
                "prefer" => collection.preference(name).map(|(name, _)| format!("prefer:{name}")),
                "include" => collection.exclusion(name).map(|(name, _)| format!("include:{name}")),
                _ => None,
            };
            let Some(canonical) = canonical else {
                return Err(fail(format!("the description of \"{term}\" has nothing to describe")));
            };
            collection.descriptions.push((canonical, text));
        }
        Ok(collection)
    }
}
