//! Query text to syntax tree, before names and values are checked against a collection.

use crate::ast::{Issue, QueryError, Span};
use crate::schema::Op;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RawKind {
    Word,
    Quoted,
    Regex,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RawValue {
    pub kind: RawKind,
    pub text: String,
}

impl RawValue {
    pub fn word(text: impl Into<String>) -> RawValue {
        RawValue { kind: RawKind::Word, text: text.into() }
    }

    pub fn quoted(text: impl Into<String>) -> RawValue {
        RawValue { kind: RawKind::Quoted, text: text.into() }
    }

    pub fn regex(text: impl Into<String>) -> RawValue {
        RawValue { kind: RawKind::Regex, text: text.into() }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Cst {
    And(Vec<Cst>),
    Or(Vec<Cst>),
    Not(Box<Cst>),
    Term { key: String, op: Op, value: RawValue, span: Span },
    Bare { value: RawValue, exact: bool, span: Span },
}

/// Operators, longest first so that `<=` is not read as `<`.
const OPS: [(&str, Op); 7] = [
    ("!=", Op::Ne),
    ("<=", Op::Le),
    (">=", Op::Ge),
    (":", Op::Colon),
    ("=", Op::Eq),
    ("<", Op::Lt),
    (">", Op::Gt),
];

/// Parses a query string into a syntax tree. An empty query is an empty AND.
///
/// Terms separated by spaces are ANDed, `or` separates alternatives and binds more
/// loosely than AND, `-` negates the term or group that follows, and parentheses group.
pub fn parse_cst(input: &str) -> Result<Cst, QueryError> {
    let mut parser = Parser { chars: input.chars().collect(), at: 0 };
    if parser.chars.iter().all(|ch| ch.is_whitespace()) {
        return Ok(Cst::And(Vec::new()));
    }
    let root = parser.or()?;
    parser.skip_space();
    if parser.at < parser.chars.len() {
        return Err(syntax("Unmatched closing parenthesis", parser.at, parser.at + 1));
    }
    Ok(root)
}

fn syntax(message: impl Into<String>, start: usize, end: usize) -> QueryError {
    QueryError {
        issues: vec![Issue { code: "syntax", message: message.into(), span: Some(Span { start, end }) }],
    }
}

struct Parser {
    chars: Vec<char>,
    at: usize,
}

impl Parser {
    fn peek(&self) -> Option<char> {
        self.chars.get(self.at).copied()
    }

    fn skip_space(&mut self) {
        while self.peek().is_some_and(char::is_whitespace) {
            self.at += 1;
        }
    }

    /// The run of characters up to the next space or parenthesis, without consuming it.
    fn word(&self) -> String {
        self.chars[self.at..]
            .iter()
            .take_while(|ch| !ch.is_whitespace() && **ch != '(' && **ch != ')')
            .collect()
    }

    fn or(&mut self) -> Result<Cst, QueryError> {
        let mut children = vec![self.and()?];
        while self.word().eq_ignore_ascii_case("or") {
            self.at += 2;
            children.push(self.and()?);
        }
        Ok(if children.len() == 1 { children.remove(0) } else { Cst::Or(children) })
    }

    fn and(&mut self) -> Result<Cst, QueryError> {
        let mut children = Vec::new();
        loop {
            self.skip_space();
            if self.at >= self.chars.len() || self.peek() == Some(')') {
                break;
            }
            let word = self.word();
            if word.eq_ignore_ascii_case("or") {
                break;
            }
            if word.eq_ignore_ascii_case("and") {
                self.at += 3;
                continue;
            }
            children.push(self.unary()?);
        }
        if children.is_empty() {
            let at = self.at.min(self.chars.len());
            return Err(syntax("Expected a search term", at, at + 1));
        }
        Ok(if children.len() == 1 { children.remove(0) } else { Cst::And(children) })
    }

    fn unary(&mut self) -> Result<Cst, QueryError> {
        let start = self.at;
        let next = self.chars.get(self.at + 1);
        if self.peek() == Some('-') && next.is_some_and(|ch| !ch.is_whitespace()) {
            self.at += 1;
            return Ok(Cst::Not(Box::new(self.unary()?)));
        }
        if self.peek() == Some('(') {
            self.at += 1;
            let group = self.or()?;
            self.skip_space();
            if self.peek() != Some(')') {
                return Err(syntax("Unclosed parenthesis", start, start + 1));
            }
            self.at += 1;
            return Ok(group);
        }
        self.term()
    }

    fn term(&mut self) -> Result<Cst, QueryError> {
        let start = self.at;
        if let Some((key, op, width)) = self.key_and_op() {
            self.at += width;
            let value = self.value(true)?;
            if value.kind == RawKind::Word && value.text.is_empty() {
                let written: String = self.chars[start..start + width].iter().collect();
                return Err(syntax(format!("Expected a value after \"{written}\""), start, self.at));
            }
            let span = Span { start, end: self.at };
            return Ok(Cst::Term { key, op, value, span });
        }
        let exact = self.peek() == Some('!');
        if exact {
            self.at += 1;
        }
        let value = self.value(false)?;
        if value.text.is_empty() {
            return Err(syntax("Expected a word or phrase", start, self.at + 1));
        }
        Ok(Cst::Bare { value, exact, span: Span { start, end: self.at } })
    }

    /// An identifier directly followed by an operator, and how many characters they take.
    fn key_and_op(&self) -> Option<(String, Op, usize)> {
        let rest = &self.chars[self.at..];
        let first = *rest.first()?;
        if !(first.is_ascii_alphabetic() || first == '_') {
            return None;
        }
        let length = rest.iter().take_while(|ch| ch.is_ascii_alphanumeric() || **ch == '_').count();
        let after: String = rest[length..].iter().take(2).collect();
        let (text, op) = OPS.iter().find(|(text, _)| after.starts_with(text))?;
        Some((rest[..length].iter().collect(), *op, length + text.len()))
    }

    fn value(&mut self, allow_regex: bool) -> Result<RawValue, QueryError> {
        match self.peek() {
            Some('"') => Ok(RawValue::quoted(self.delimited('"', "quoted phrase")?)),
            Some('/') if allow_regex => Ok(RawValue::regex(self.delimited('/', "regular expression")?)),
            _ => {
                let text = self.word();
                self.at += text.chars().count();
                Ok(RawValue::word(text))
            }
        }
    }

    /// Reads up to the closing delimiter. A backslash escapes the delimiter and, in a
    /// quoted phrase, itself. A regex keeps every other escape (`\d`, `\b`, `\\`) intact.
    fn delimited(&mut self, close: char, what: &str) -> Result<String, QueryError> {
        let start = self.at;
        self.at += 1;
        let mut text = String::new();
        while let Some(ch) = self.peek() {
            self.at += 1;
            if ch == close {
                return Ok(text);
            }
            match (ch, self.peek()) {
                ('\\', Some(next)) => {
                    self.at += 1;
                    if next != close && !(close == '"' && next == '\\') {
                        text.push(ch);
                    }
                    text.push(next);
                }
                _ => text.push(ch),
            }
        }
        Err(syntax(format!("Unterminated {what}"), start, self.chars.len()))
    }
}
