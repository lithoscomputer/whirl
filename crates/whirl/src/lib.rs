//! Whirl: a CLI that runs web UI tests written in plain-text `.whirl` files.
//!
//! This crate hosts the command-line surface and the runner. The `.whirl`
//! language lives in `whirl-lang`, the check engine in `whirl-check`, the
//! report model and renderers in `whirl-report`, the browser shim client
//! in `whirl-shim`, and the language model calls in `whirl-ai`. `SPEC.md`
//! at the repository root is the product authority for everything in here.

mod cli;
mod doctor;
mod install;
mod run;
mod telemetry;

// The binary only needs the command entry point.
pub use cli::run;
