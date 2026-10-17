#![doc = include_str!("../README.md")]

pub mod ast;
pub mod backend;
pub mod collection;
pub mod cursor;
pub mod http;
pub mod parser;
pub mod plan;
pub mod print;
pub mod resolve;
pub mod schema;
pub mod semantics;
pub mod syntax;
pub mod time;

pub use ast::{Direction, Issue, Node, Query, QueryError, Span, Value};
pub use backend::memory::MemoryBackend;
pub use backend::{Backend, Error, Page, PageRequest, Row};
pub use collection::{Collection, CollectionBuilder, DefinitionError, Order};
pub use parser::RawValue;
pub use print::{print, print_node};
pub use resolve::{Directives, ParseOptions, compare, parse};
pub use schema::{Field, FieldKind, Op, SetColon};
