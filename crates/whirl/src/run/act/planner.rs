//! The seam that chooses an `ACT` action (SPEC 7.4). A planner turns the
//! instruction and a page snapshot into the model's answer. The caller
//! takes the snapshot, checks the answer with [`PageSnapshot::decide`], and
//! runs the action, so every planner shares that execution.

use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Instant;

use lithos_llm::types::Usage;

use crate::run::act::decision::ActInference;
use crate::run::act::instruction::Instruction;
use crate::run::act::model::ModelClient;
use crate::run::act::prompt;
use crate::run::act::snapshot::PageSnapshot;

/// Which planning step of one `ACT` line a request is for.
#[derive(Clone, Copy, Debug)]
pub(crate) enum PlanStep<'a> {
    First,
    /// Step two of a two-step action. The text describes the action that
    /// step one ran.
    Second {
        first_action: &'a str,
    },
}

/// What one planning step needs.
#[derive(Clone, Copy, Debug)]
pub(crate) struct PlanRequest<'a> {
    pub(crate) instruction: &'a Instruction,
    pub(crate) snapshot:    &'a PageSnapshot,
    pub(crate) step:        PlanStep<'a>,
    /// The file's `model` option.
    pub(crate) model:       &'a str,
    /// When planning must end, retries included.
    pub(crate) deadline:    Instant,
    /// Likely elements another planner found, which the prompt shows the
    /// model before the snapshot.
    pub(crate) hint:        Option<&'a str>,
}

/// What one planning step spent.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct PlanUsage {
    pub(crate) model_calls: u32,
    pub(crate) model:       Usage,
    pub(crate) jev:         JevUsage,
}

/// What Jev requests used.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct JevUsage {
    pub(crate) requests:        u32,
    pub(crate) input_tokens:    u64,
    pub(crate) output_tokens:   u64,
    pub(crate) cost_usd_micros: u64,
    /// True when the catalog could not price an answered request.
    pub(crate) unpriced:        bool,
}

impl JevUsage {
    /// The requests' cost, when every answered one was priced.
    pub(crate) fn cost(self) -> Option<u64> {
        (!self.unpriced).then_some(self.cost_usd_micros)
    }
}

impl PlanUsage {
    pub(crate) fn saturating_add(self, other: Self) -> Self {
        Self {
            model_calls: self.model_calls.saturating_add(other.model_calls),
            model:       self.model.saturating_add(other.model),
            jev:         JevUsage {
                requests:        self.jev.requests.saturating_add(other.jev.requests),
                input_tokens:    self.jev.input_tokens.saturating_add(other.jev.input_tokens),
                output_tokens:   self
                    .jev
                    .output_tokens
                    .saturating_add(other.jev.output_tokens),
                cost_usd_micros: self
                    .jev
                    .cost_usd_micros
                    .saturating_add(other.jev.cost_usd_micros),
                unpriced:        self.jev.unpriced || other.jev.unpriced,
            },
        }
    }
}

/// Which planner chose an answer.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum PlannedBy {
    Llm,
    Jev,
}

impl PlannedBy {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Llm => "llm",
            Self::Jev => "jev",
        }
    }
}

/// Why planning gave no answer.
#[derive(Debug, thiserror::Error)]
pub(crate) enum PlanError {
    /// The model call failed; the caller classifies it (SPEC 7.4).
    #[error(transparent)]
    Model(lithos_llm::Error),
    #[error("the model's answer does not match the ACT schema: {0}")]
    Answer(serde_json::Error),
}

/// One planning step's result. The usage counts even when planning fails.
#[derive(Debug)]
pub(crate) struct Plan {
    pub(crate) answer:     Result<ActInference, PlanError>,
    pub(crate) usage:      PlanUsage,
    pub(crate) planned_by: PlannedBy,
}

/// The future a planner returns. It is boxed, so the trait is
/// dyn-compatible.
pub(crate) type PlanFuture<'a> = Pin<Box<dyn Future<Output = Plan> + Send + 'a>>;

/// Chooses the action for one `ACT` planning step.
///
/// An implementation returns the model's answer or a [`PlanError`], and
/// always the usage it spent. It must not touch the page, and it must end
/// by `request.deadline`. The run shares one planner across its workers.
pub(crate) trait ActPlanner: fmt::Debug + Send + Sync {
    fn plan<'a>(&'a self, request: PlanRequest<'a>) -> PlanFuture<'a>;

    /// The name reports give this planner, such as `llm`.
    fn name(&self) -> &'static str;
}

/// Plans with one structured call to the `model` option's language model.
#[derive(Debug)]
pub(crate) struct LlmPlanner {
    client: Arc<ModelClient>,
}

impl LlmPlanner {
    pub(crate) fn new(client: Arc<ModelClient>) -> Self {
        Self { client }
    }
}

impl ActPlanner for LlmPlanner {
    fn plan<'a>(&'a self, request: PlanRequest<'a>) -> PlanFuture<'a> {
        Box::pin(async move {
            let instruction = request.instruction;
            let placeholders = instruction.bindings().placeholders();
            let prompt = match request.step {
                PlanStep::First => prompt::act_prompt(instruction.prompt(), &placeholders),
                PlanStep::Second { first_action } => {
                    prompt::step_two_prompt(instruction.prompt(), first_action, &placeholders)
                }
            };
            let user = prompt::user_message(&prompt, request.hint, request.snapshot.text());
            let reply = self
                .client
                .plan(
                    request.model,
                    &prompt::system_prompt(),
                    &user,
                    request.deadline,
                )
                .await;
            let (answer, model) = match reply {
                Ok(reply) => (reply.answer.map_err(PlanError::Answer), reply.usage),
                Err(error) => (Err(PlanError::Model(error)), Usage::default()),
            };
            Plan {
                answer,
                usage: PlanUsage {
                    model_calls: 1,
                    model,
                    jev: JevUsage::default(),
                },
                planned_by: PlannedBy::Llm,
            }
        })
    }

    fn name(&self) -> &'static str {
        PlannedBy::Llm.as_str()
    }
}
