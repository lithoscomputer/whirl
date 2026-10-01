//! Whirl's language model calls. One model, named by the `model` option
//! (SPEC 5), answers five kinds of question: `ACT` (SPEC 7.4) and `GOAL`
//! (SPEC 7.7) choose element actions from an AI snapshot of the selected
//! window, an `ai:` target (SPEC 6.3) finds the elements a description
//! names, `EXTRACT` (SPEC 7.6) reads a typed value from the page, and
//! `JUDGE` (SPEC 9.8) answers a claim about it. This crate owns the model
//! catalog and client, the prompts, the snapshot parser, the decision that
//! turns an answer into a Whirl action, and, with `--jev`, the Jev planner.
//! The runner takes the snapshots and runs the chosen actions.

mod decision;
mod instruction;
mod jev;
mod model;
mod planner;
mod prompt;
mod snapshot;

pub use decision::{ActDecision, FollowUp, GoalStatus, PlannedAction};
pub use instruction::{Instruction, Variables};
pub use jev::{JevClient, JevPlanner, JevSetupError};
/// A failed model call. The runner classifies it by [`ModelErrorKind`]
/// (SPEC 7.4): content filtering and an input larger than the context fail
/// the entry, and other kinds are runtime errors.
pub use lithos_llm::Error as ModelError;
/// The kind of a [`ModelError`].
pub use lithos_llm::types::ErrorKind as ModelErrorKind;
pub use model::{JudgeAnswer, ModelCatalog, ModelClient, ModelSetupError, Verdict};
pub use planner::{ActPlanner, LlmPlanner, PlanError, PlanRequest, PlanStep, PlanUsage, PlannedBy};
pub use prompt::{extract_message, goal_message, judge_message, target_message};
pub use snapshot::{Fingerprint, PageSnapshot, Target};
