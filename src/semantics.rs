//! What operators mean, shared by every backend so that they cannot disagree.

use std::cmp::Ordering;

use crate::schema::{Field, FieldKind, Op, SetColon};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CompareOp {
    Eq,
    Lt,
    Le,
    Gt,
    Ge,
}

impl CompareOp {
    pub fn as_str(self) -> &'static str {
        match self {
            CompareOp::Eq => "=",
            CompareOp::Lt => "<",
            CompareOp::Le => "<=",
            CompareOp::Gt => ">",
            CompareOp::Ge => ">=",
        }
    }

    /// Whether an ordering of `left` against `right` satisfies `left op right`.
    pub fn holds(self, ordering: Ordering) -> bool {
        match self {
            CompareOp::Eq => ordering == Ordering::Equal,
            CompareOp::Lt => ordering == Ordering::Less,
            CompareOp::Le => ordering != Ordering::Greater,
            CompareOp::Gt => ordering == Ordering::Greater,
            CompareOp::Ge => ordering != Ordering::Less,
        }
    }
}

/// What an operator means on a field, once the field-specific meaning of `:` is applied.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Effective {
    /// A case-insensitive substring match, or a regular expression match.
    Contains,
    /// The complement of [`CompareOp::Eq`]: it is true of a null.
    Ne,
    Compare(CompareOp),
}

pub fn effective_op(field: &Field, op: Op) -> Effective {
    Effective::Compare(match op {
        Op::Colon => match field.kind {
            FieldKind::Text => return Effective::Contains,
            FieldKind::Set => match field.colon {
                SetColon::HasAll => CompareOp::Ge,
                SetColon::Within => CompareOp::Le,
                SetColon::Exactly => CompareOp::Eq,
            },
            _ => CompareOp::Eq,
        },
        Op::Ne => return Effective::Ne,
        Op::Eq => CompareOp::Eq,
        Op::Lt => CompareOp::Lt,
        Op::Le => CompareOp::Le,
        Op::Gt => CompareOp::Gt,
        Op::Ge => CompareOp::Ge,
    })
}

/// Turns a comparison against a date literal into plain string comparisons, all of which
/// must hold. A literal covers the range `[start, end)`, so `=2026` means "within 2026"
/// and `>2026` means "after the end of 2026". `now` is an instant: its range is empty and
/// it compares directly.
pub fn date_bounds<'a>(op: CompareOp, start: &'a str, end: &'a str) -> Vec<(CompareOp, &'a str)> {
    if start == end {
        return vec![(op, start)];
    }
    match op {
        CompareOp::Eq => vec![(CompareOp::Ge, start), (CompareOp::Lt, end)],
        CompareOp::Lt => vec![(CompareOp::Lt, start)],
        CompareOp::Le => vec![(CompareOp::Lt, end)],
        CompareOp::Gt => vec![(CompareOp::Ge, end)],
        CompareOp::Ge => vec![(CompareOp::Ge, start)],
    }
}

/// The members of an ordinal field that satisfy `member op value`, by rank.
pub fn ordinal_matches<'a>(field: &'a Field, op: CompareOp, value: &str) -> Vec<&'a str> {
    let Some(rank) = field.rank(value) else {
        return Vec::new();
    };
    field
        .values
        .iter()
        .enumerate()
        .filter(|(index, _)| op.holds(index.cmp(&rank)))
        .map(|(_, member)| member.as_str())
        .collect()
}
