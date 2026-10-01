//! Whirl: a CLI that runs web UI tests written in plain-text `.whirl` files.
//!
//! This crate hosts the command-line surface, the check engine, the runner,
//! and the reporters. The `.whirl` language lives in `whirl-lang`. `SPEC.md`
//! at the repository root is the product authority for everything in here.

mod check;
mod cli;
mod doctor;
mod install;
mod report;
mod run;
mod telemetry;

// The binary only needs the command entry point.
pub use cli::run;
