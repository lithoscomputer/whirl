//! Whirl: a CLI that runs web UI tests written in plain-text `.whirl` files.
//!
//! This crate hosts the `.whirl` language (AST, parser, and later the
//! formatter and lint rules), the runner, and the reporters. `SPEC.md` at the
//! repository root is the product authority for everything in here.

pub mod cli;
pub mod lang;
