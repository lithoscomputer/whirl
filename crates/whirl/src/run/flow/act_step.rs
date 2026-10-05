//! One `ACT` line (SPEC 7.4): take a snapshot, ask the model, check its
//! answer, and run the chosen action as an ordinary shim command. A
//! two-step action plans once more on a fresh snapshot. The whole line
//! shares one step budget (SPEC 12).

use std::error::Error as _;
use std::time::{Duration, Instant};

use lithos_llm::types::ErrorKind;
use serde_json::Value as Json;

use super::{EntryState, FlowExec, StepBudget, StepEnd, StepNode, entry_timeout_error};
use crate::report::model::{ActActionReport, ActJevUsage, ActReport, ActUsage, StepError};
use crate::run::act::{
    ActDecision, FollowUp, Instruction, PageSnapshot, PlanError, PlanRequest, PlanStep, PlanUsage,
    PlannedBy,
};
use crate::run::shim::{
    AriaSnapshotResult, ReadResult, ShimClient, StepCommand, StepOutcome, StepRequest,
};

/// The budget of one `ACT` line.
#[derive(Clone, Copy, Debug)]
pub(super) struct ActBudget {
    pub(super) timeout_ms:      u64,
    pub(super) entry_capped:    bool,
    pub(super) entry_budget_ms: u64,
}

/// What stays fixed while one `ACT` line runs its shim calls.
struct ActLine<'a> {
    node:        StepNode<'a>,
    title:       &'a str,
    deadline:    Instant,
    budget:      ActBudget,
    /// True until the line's first shim call, which starts the entry's
    /// observation windows when the line is the entry's first step.
    entry_start: bool,
}

impl ActLine<'_> {
    fn remaining_ms(&self) -> u64 {
        u64::try_from(
            self.deadline
                .saturating_duration_since(Instant::now())
                .as_millis(),
        )
        .unwrap_or(u64::MAX)
    }

    /// The failure when the line's budget runs out (SPEC 12).
    fn timed_out(&self) -> StepEnd {
        if self.budget.entry_capped {
            return StepEnd::Failed(entry_timeout_error(self.budget.entry_budget_ms));
        }
        StepEnd::Failed(StepError {
            code: "timeout".to_owned(),
            message: format!(
                "timeout: ACT did not finish within {}ms",
                self.budget.timeout_ms
            ),
            ..StepError::default()
        })
    }
}

/// A failed shim call inside an `ACT` line: its classified end, and the
/// kind of the shim's error answer, if the shim answered.
struct ShimFailure {
    end:        StepEnd,
    shim_error: Option<String>,
}

impl ShimFailure {
    /// Whether the call failed only because the page replaced the snapshot
    /// element.
    fn stale_ref(&self) -> bool {
        self.shim_error.as_deref() == Some("stale-ref")
    }
}

fn act_failure(code: &str, message: &str) -> StepError {
    StepError {
        code: code.to_owned(),
        message: format!("{code}: {message}"),
        ..StepError::default()
    }
}

/// Token usage for the report: cached prompt tokens count as input, and
/// reasoning tokens as output. Jev's usage appears only with `--jev`, and
/// the step's cost includes Jev's.
fn usage_report(usage: PlanUsage, jev_planner: bool) -> ActUsage {
    let PlanUsage {
        model_calls,
        model,
        jev,
    } = usage;
    let tokens = model.tokens;
    ActUsage {
        model_calls,
        input_tokens: tokens
            .input
            .saturating_add(tokens.cache_read)
            .saturating_add(tokens.cache_write),
        output_tokens: tokens.output.saturating_add(tokens.reasoning),
        cost_usd_micros: step_cost(
            model_calls,
            model.cost.map(|cost| cost.usd_micros),
            jev.cost(),
        ),
        jev: jev_planner.then_some(ActJevUsage {
            requests:        jev.requests,
            input_tokens:    jev.input_tokens,
            output_tokens:   jev.output_tokens,
            cost_usd_micros: jev.cost(),
        }),
    }
}

/// A step's cost: the model calls' and Jev's, when both are known. A step
/// Jev planned alone made no model call, so the model's part is 0.
fn step_cost(model_calls: u32, model: Option<u64>, jev: Option<u64>) -> Option<u64> {
    let model = if model_calls == 0 { Some(0) } else { model };
    Some(model?.saturating_add(jev?))
}

impl FlowExec<'_> {
    /// Runs one `ACT` line and reports what it did, pass or fail.
    pub(super) async fn run_act(
        &mut self,
        node: StepNode<'_>,
        instruction: &Instruction,
        scope: Option<Json>,
        title: &str,
        budget: ActBudget,
        client: &mut ShimClient,
        state: &mut EntryState,
    ) -> (StepEnd, Option<ActReport>) {
        let (Some(planner), Some(model)) = (self.run.planner, self.options.model.clone()) else {
            let error = act_failure("act-model", "ACT needs the model option and a planner");
            return (StepEnd::Error(error), None);
        };
        let mut line = ActLine {
            node,
            title,
            deadline: Instant::now() + Duration::from_millis(budget.timeout_ms),
            budget,
            entry_start: state.steps.is_empty(),
        };
        let mut actions = Vec::new();
        let mut usage = PlanUsage::default();
        // Step two's prompt describes the action step one ran.
        let mut first_action: Option<String> = None;
        // A page can replace the chosen element while the model answers.
        // Whirl then asks once more on a fresh snapshot (SPEC 7.4).
        let mut stale_retry_left = true;

        let end = loop {
            let snapshot = match self
                .act_shim_call(
                    &mut line,
                    StepCommand::AriaSnapshot {
                        locator: scope.clone(),
                    },
                    client,
                    state,
                )
                .await
            {
                Ok(result) => match serde_json::from_value::<AriaSnapshotResult>(result) {
                    Ok(result) if scope.is_none() => {
                        PageSnapshot::parse(&result.snapshot).of_page()
                    }
                    Ok(result) => PageSnapshot::parse(&result.snapshot),
                    Err(_) => {
                        break StepEnd::Error(act_failure(
                            "internal",
                            "malformed ariaSnapshot result from the shim",
                        ));
                    }
                },
                Err(failure) => break failure.end,
            };

            let step = match &first_action {
                None => PlanStep::First,
                Some(first) => PlanStep::Second {
                    first_action: first,
                },
            };
            let plan = planner
                .plan(PlanRequest {
                    instruction,
                    snapshot: &snapshot,
                    step,
                    model: &model,
                    deadline: line.deadline,
                    hint: None,
                })
                .await;
            usage = usage.saturating_add(plan.usage);
            let planned_by = plan.planned_by;
            let inference = match plan.answer {
                Ok(inference) => inference,
                Err(PlanError::Model(error)) => break self.model_failure(&error, &line),
                Err(error @ PlanError::Answer(_)) => {
                    break StepEnd::Failed(act_failure("act-invalid-decision", &error.to_string()));
                }
            };
            let (action, description, then) = match snapshot.decide(inference, instruction) {
                Ok(ActDecision::Perform {
                    action,
                    description,
                    then,
                }) => (action, description, then),
                // When step two finds nothing, the line passes with the
                // first action (SPEC 7.4).
                Ok(ActDecision::NoMatch) if first_action.is_some() => break StepEnd::Passed,
                Ok(ActDecision::NoMatch) => {
                    break StepEnd::Failed(act_failure(
                        "act-no-match",
                        &format!(
                            "no element on the page matches \"{}\"",
                            self.vars.mask(instruction.prompt())
                        ),
                    ));
                }
                Err(error) => {
                    break StepEnd::Failed(act_failure(
                        "act-invalid-decision",
                        &self.vars.mask(&error.to_string()),
                    ));
                }
            };

            match self
                .act_shim_call(&mut line, action.command(instruction), client, state)
                .await
            {
                Ok(_) => {}
                Err(failure) if failure.stale_ref() && stale_retry_left => {
                    stale_retry_left = false;
                    continue;
                }
                Err(failure) => break failure.end,
            }
            actions.push(ActActionReport {
                line:        self.vars.mask(&action.line()),
                description: self.vars.mask(&description),
                planned_by:  planned_by.as_str().to_owned(),
            });
            if let Some(read_back) = action.fill_read_back(instruction) {
                match self
                    .read_back_value(&mut line, read_back.command.clone(), client, state)
                    .await
                {
                    Ok(Some(held)) if !read_back.matches(&held) => {
                        break StepEnd::Failed(act_failure(
                            "act-fill-mismatch",
                            &self.vars.mask(&read_back.mismatch(&action, &held)),
                        ));
                    }
                    Ok(_) => {}
                    Err(failure) => break failure.end,
                }
            }
            match then {
                FollowUp::Replan if first_action.is_none() => {
                    first_action = Some(action.describe_for_model(&description));
                }
                FollowUp::Replan | FollowUp::Done => break StepEnd::Passed,
            }
        };
        let jev_planner = planner.name() == PlannedBy::Jev.as_str();
        let report = ActReport {
            model,
            planner: planner.name().to_owned(),
            actions,
            usage: usage_report(usage, jev_planner),
        };
        (end, Some(report))
    }

    /// Runs one shim command inside the line's remaining budget. A
    /// failure is classified like any step's (protocol section 7).
    async fn act_shim_call(
        &mut self,
        line: &mut ActLine<'_>,
        command: StepCommand,
        client: &mut ShimClient,
        state: &mut EntryState,
    ) -> Result<Json, ShimFailure> {
        let timeout_ms = line.remaining_ms();
        if timeout_ms == 0 {
            return Err(ShimFailure {
                end:        line.timed_out(),
                shim_error: None,
            });
        }
        let request = StepRequest {
            entry_start: line.entry_start,
            command,
            timeout_ms,
            title: Some(line.title.to_owned()),
        };
        line.entry_start = false;
        let started = Instant::now();
        match client.run_step(&request).await {
            StepOutcome::Ok(result) => Ok(result),
            outcome => {
                let budget = StepBudget {
                    entry_capped: line.budget.entry_capped,
                    entry_budget_ms: line.budget.entry_budget_ms,
                    timeout_ms,
                    elapsed: started.elapsed(),
                };
                let shim_error = match &outcome {
                    StepOutcome::ShimError(error) => Some(error.kind.clone()),
                    _ => None,
                };
                Err(ShimFailure {
                    end: self.apply_outcome(line.node, outcome, state, budget),
                    shim_error,
                })
            }
        }
    }

    /// Reads the value a fill left in its field. `None` means Whirl cannot
    /// tell: the budget is spent, the element is gone, or it has no value,
    /// such as a `contenteditable` element.
    async fn read_back_value(
        &mut self,
        line: &mut ActLine<'_>,
        command: StepCommand,
        client: &mut ShimClient,
        state: &mut EntryState,
    ) -> Result<Option<String>, ShimFailure> {
        if line.remaining_ms() == 0 {
            return Ok(None);
        }
        match self.act_shim_call(line, command, client, state).await {
            Ok(result) => match serde_json::from_value::<ReadResult>(result) {
                Ok(ReadResult::Value {
                    value: Json::String(held),
                }) => Ok(Some(held)),
                _ => Ok(None),
            },
            Err(failure) if failure.shim_error.is_some() => Ok(None),
            Err(failure) => Err(failure),
        }
    }

    /// Classifies a failed model call (SPEC 7.4). A call that ran out the
    /// line's budget is a timeout like any other step's.
    fn model_failure(&self, error: &lithos_llm::Error, line: &ActLine<'_>) -> StepEnd {
        if line.remaining_ms() == 0 {
            return line.timed_out();
        }
        let mut message = format!("{}: {error}", error.kind().as_str());
        let mut source = error.source();
        while let Some(cause) = source {
            message.push_str(": ");
            message.push_str(&cause.to_string());
            source = cause.source();
        }
        let message = self.vars.mask(&message);
        match error.kind() {
            ErrorKind::ContentFilter | ErrorKind::ContextLength => {
                StepEnd::Failed(act_failure("act-model", &message))
            }
            ErrorKind::ResponseDecode => {
                StepEnd::Failed(act_failure("act-invalid-decision", &message))
            }
            _ => StepEnd::Error(act_failure("act-model", &message)),
        }
    }
}
