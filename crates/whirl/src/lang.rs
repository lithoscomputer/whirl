//! The `.whirl` language: typed AST, line-oriented parser, canonical
//! formatter, lint rules, and shim wire conversion.

pub(crate) mod ast;
pub(crate) mod fmt;
pub(crate) mod lint;
pub(crate) mod parse;
pub(crate) mod wire;
