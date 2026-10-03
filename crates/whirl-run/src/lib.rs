//! Whirl's runner. Given parsed `.whirl` files, it runs each flow through
//! one shim process (SPEC 12): option resolution, entry and step
//! execution with timeout budgeting, failure artifacts, and the per-file
//! report. Around that it owns the worker pool and setup scheduling, the
//! variables and secret masking (SPEC 10, 11), the AI cache (SPEC 12.1),
//! and the mapping from flows to artifact directories (SPEC 14). It
//! builds on the shim client in `whirl-shim`, the planner in `whirl-ai`,
//! the check engine in `whirl-check`, and the report model in
//! `whirl-report`.
//!
//! The entry point is [`run_files`], configured by [`RunSettings`] and
//! [`FlowFlags`]. The CLI also reaches the pieces it needs before a run:
//! input dedup, setup-flow discovery, `--var` parsing, and the [`cache`]
//! diagnostics of `whirl check`.

mod artifacts;
pub mod cache;
mod extract;
mod flow;
mod runner;
mod vars;

pub use artifacts::{ArtifactsError, dedup_flows};
pub use flow::{FlowFlags, setup_path_for};
pub use runner::{RunSettings, RunnerError, run_files};
pub use vars::{VarsFileError, parse_var_flag, parse_variables_file};
