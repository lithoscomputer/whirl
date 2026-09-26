//! `ACT` (SPEC 7.4): a language model chooses one element action from an
//! AI snapshot of the selected tab, and Whirl runs it as an ordinary
//! action. The design follows Stagehand's `act()`, without its cache.

mod decision;
mod instruction;
mod model;
mod prompt;
mod snapshot;

pub(crate) use decision::{ActDecision, FollowUp};
pub(crate) use instruction::Instruction;
pub(crate) use model::{ModelCatalog, ModelClient, ModelSetupError};
pub(crate) use snapshot::PageSnapshot;

/// The system prompt, the first planning prompt, and the step-two prompt.
pub(crate) mod prompts {
    pub(crate) use super::prompt::{act_prompt, step_two_prompt, system_prompt, user_message};
}
