//! `ACT` (SPEC 7.4): a language model chooses one element action from an
//! AI snapshot of the selected tab, and Whirl runs it as an ordinary
//! action. The design follows Stagehand's `act()`, without its cache.

mod decision;
mod instruction;
mod jev;
mod model;
mod planner;
mod prompt;
mod snapshot;

pub(crate) use decision::{ActDecision, FollowUp};
pub(crate) use instruction::Instruction;
pub(crate) use jev::{JevClient, JevPlanner, JevSetupError};
pub(crate) use model::{ModelCatalog, ModelClient, ModelSetupError};
pub(crate) use planner::{
    ActPlanner, LlmPlanner, PlanError, PlanRequest, PlanStep, PlanUsage, PlannedBy,
};
pub(crate) use snapshot::PageSnapshot;
