//! The `.whirl` language: typed AST, line-oriented parser, canonical
//! formatter, and lint rules.

pub(crate) mod ast;
pub(crate) mod cli_options;
pub(crate) mod fmt;
pub(crate) mod lint;
pub(crate) mod parse;
pub(crate) mod schema;

#[cfg(test)]
mod grammar_tests;
