//! Field kinds and how a field is declared.

/// A field's kind decides which operators and values are legal for it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum FieldKind {
    /// Free text. `:` is a case-insensitive substring match, `=` a case-insensitive
    /// match of the whole value.
    Text,
    /// An opaque token such as an id, a slug or an unordered enum. Equality only.
    Keyword,
    Number,
    /// An enum whose values are ranked in declaration order, lowest first.
    Ordinal,
    /// A set of enum members, compared by subset and superset, or by size with a number.
    Set,
    /// An ISO-8601 UTC date (`YYYY-MM-DD`) or timestamp, held as a string.
    Date,
}

impl FieldKind {
    pub fn name(self) -> &'static str {
        match self {
            FieldKind::Text => "text",
            FieldKind::Keyword => "keyword",
            FieldKind::Number => "number",
            FieldKind::Ordinal => "ordinal",
            FieldKind::Set => "set",
            FieldKind::Date => "date",
        }
    }

    /// Whether rows can be sorted or grouped on a field of this kind.
    pub fn sortable(self) -> bool {
        self != FieldKind::Set
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Op {
    /// `:`, the natural match for the field's kind.
    Colon,
    Eq,
    Ne,
    Lt,
    Gt,
    Le,
    Ge,
}

impl Op {
    pub fn as_str(self) -> &'static str {
        match self {
            Op::Colon => ":",
            Op::Eq => "=",
            Op::Ne => "!=",
            Op::Lt => "<",
            Op::Gt => ">",
            Op::Le => "<=",
            Op::Ge => ">=",
        }
    }
}

/// How `:` behaves on a set field.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SetColon {
    /// The field has every given member. The default.
    HasAll,
    /// The field has no member outside the given ones.
    Within,
    Exactly,
}

/// The declaration of one field. Build one with a constructor such as [`Field::text`]
/// and the chained setters.
#[derive(Clone, Debug)]
pub struct Field {
    pub kind: FieldKind,
    pub nullable: bool,
    pub aliases: Vec<String>,
    /// The backend column name. Defaults to the field's name.
    pub column: Option<String>,
    /// Text only: whether `field:/regex/` is accepted. Off unless asked for.
    pub regex: bool,
    /// The members of an ordinal or set field, or the allowed values of a keyword
    /// field. An empty list on a keyword field means any value is allowed.
    pub values: Vec<String>,
    /// Set only: named groups of members, usable wherever a member is.
    pub nicknames: Vec<(String, Vec<String>)>,
    /// Set only.
    pub colon: SetColon,
    /// What the field holds, in plain words, for the syntax guide.
    pub description: Option<String>,
}

impl Field {
    fn new(kind: FieldKind) -> Field {
        Field {
            kind,
            nullable: false,
            aliases: Vec::new(),
            column: None,
            regex: false,
            values: Vec::new(),
            nicknames: Vec::new(),
            colon: SetColon::HasAll,
            description: None,
        }
    }

    pub fn text() -> Field {
        Field::new(FieldKind::Text)
    }

    pub fn keyword() -> Field {
        Field::new(FieldKind::Keyword)
    }

    pub fn number() -> Field {
        Field::new(FieldKind::Number)
    }

    /// An ordinal field with its values in rank order, lowest first.
    pub fn ordinal<S: Into<String>>(values: impl IntoIterator<Item = S>) -> Field {
        Field::new(FieldKind::Ordinal).values(values)
    }

    pub fn set<S: Into<String>>(members: impl IntoIterator<Item = S>) -> Field {
        Field::new(FieldKind::Set).values(members)
    }

    pub fn date() -> Field {
        Field::new(FieldKind::Date)
    }

    /// Say what the field holds, in plain words. Shown in the syntax guide.
    pub fn describe(mut self, description: impl Into<String>) -> Field {
        self.description = Some(description.into());
        self
    }

    pub fn nullable(mut self) -> Field {
        self.nullable = true;
        self
    }

    pub fn alias(mut self, alias: impl Into<String>) -> Field {
        self.aliases.push(alias.into());
        self
    }

    pub fn column(mut self, column: impl Into<String>) -> Field {
        self.column = Some(column.into());
        self
    }

    /// Accept `field:/regex/` on a text field.
    pub fn regex(mut self) -> Field {
        self.regex = true;
        self
    }

    /// Restrict a keyword field to these values.
    pub fn values<S: Into<String>>(mut self, values: impl IntoIterator<Item = S>) -> Field {
        self.values = values.into_iter().map(Into::into).collect();
        self
    }

    /// Name a group of members of a set field.
    pub fn nickname<S: Into<String>>(
        mut self,
        name: impl Into<String>,
        members: impl IntoIterator<Item = S>,
    ) -> Field {
        self.nicknames.push((name.into(), members.into_iter().map(Into::into).collect()));
        self
    }

    pub fn colon(mut self, colon: SetColon) -> Field {
        self.colon = colon;
        self
    }

    /// Whether this field accepts an operator.
    pub fn accepts(&self, op: Op) -> bool {
        match self.kind {
            FieldKind::Text | FieldKind::Keyword => matches!(op, Op::Colon | Op::Eq | Op::Ne),
            _ => true,
        }
    }

    /// The position of a member of an ordinal field in its ranking.
    pub fn rank(&self, value: &str) -> Option<usize> {
        self.values.iter().position(|member| member == value)
    }
}
