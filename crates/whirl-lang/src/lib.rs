//! The `.whirl` language (SPEC sections 3-11, 13, 16, 17): the typed AST,
//! the line-oriented parser, the canonical formatter, the lint rules, and
//! the resolver for options set on the command line. This crate knows
//! nothing about running a flow or evaluating a check, and it builds with
//! no native code.

pub mod ast;
mod cli_options;
mod fmt;
mod lint;
mod parse;
mod schema;

#[cfg(test)]
mod grammar_tests;

pub use cli_options::{CliOptionError, CliOptions, OptionFlag};
pub use fmt::{format_file, render_action, render_snapshot_option, render_snapshot_target};
pub use lint::{
    Lint, ModelFacts, Severity, lint_act, lint_file_with, lint_setup_refs, setup_capture_uses,
};
pub use parse::{
    OPTION_KEYS, ParseError, ParseErrorCode, is_role, parse_action_line, parse_file, parse_locator,
};
