//! One `ACT` line (SPEC 7.4): take a snapshot, ask the model, check its
//! answer, and run the chosen action as an ordinary shim command. A
//! two-step action plans once more on a fresh snapshot. The whole line
//! shares one step budget (SPEC 12).

use std::error::Error as _;
use std::time::{Duration, Instant};

use lithos_llm::types::{ErrorKind, Usage};
use serde_json::Value as Json;

use super::{EntryState, FlowExec, StepBudget, StepEnd, StepNode, entry_timeout_error};
use crate::report::model::{ActActionReport, ActReport, ActUsage, StepError};
use crate::run::act::{ActDecision, FollowUp, Instruction, PageSnapshot, prompts};
use crate::run::shim::{AriaSnapshotResult, ShimClient, StepCommand, StepOutcome, StepRequest};

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

/// A failed shim call inside an `ACT` line: its classified end, and
/// whether it failed only because the page replaced the snapshot element.
struct ShimFailure {
    end:       StepEnd,
    stale_ref: bool,
}

fn act_failure(code: &str, message: &str) -> StepError {
    StepError {
        code: code.to_owned(),
        message: format!("{code}: {message}"),
        ..StepError::default()
    }
}

/// Token usage for the report: cached prompt tokens count as input, and
/// reasoning tokens as output.
fn usage_report(model_calls: u32, usage: Usage) -> ActUsage {
    let tokens = usage.tokens;
    ActUsage {
        model_calls,
        input_tokens: tokens
            .input
            .saturating_add(tokens.cache_read)
            .saturating_add(tokens.cache_write),
        output_tokens: tokens.output.saturating_add(tokens.reasoning),
        cost_usd_micros: usage.cost.map(|cost| cost.usd_micros),
    }
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
        let (Some(model_client), Some(model)) = (self.run.model, self.options.model.clone()) else {
            let error = act_failure("act-model", "ACT needs the model option and a model client");
            return (StepEnd::Error(error), None);
        };
        let mut line = ActLine {
            node,
            title,
            deadline: Instant::now() + Duration::from_millis(budget.timeout_ms),
            budget,
            entry_start: state.steps.is_empty(),
        };
        let placeholders = instruction.bindings().placeholders();
        let system = prompts::system_prompt();
        let mut actions = Vec::new();
        let mut usage = Usage::default();
        let mut model_calls = 0;
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

            let prompt = match &first_action {
                None => prompts::act_prompt(instruction.prompt(), &placeholders),
                Some(first) => prompts::step_two_prompt(instruction.prompt(), first, &placeholders),
            };
            let user = prompts::user_message(&prompt, snapshot.text());
            model_calls += 1;
            let reply = match model_client
                .plan(&model, &system, &user, line.deadline)
                .await
            {
                Ok(reply) => reply,
                Err(error) => break self.model_failure(&error, &line),
            };
            usage = usage.saturating_add(reply.usage);
            let inference = match reply.answer {
                Ok(inference) => inference,
                Err(error) => {
                    break StepEnd::Failed(act_failure(
                        "act-invalid-decision",
                        &format!("the model's answer does not match the ACT schema: {error}"),
                    ));
                }
            };
            let (action, description, then) = match snapshot.decide(inference, instruction) {
                Ok(ActDecision::Perform {
                    action,
                    description,
                    then,
                }) => (action, description, then),
                // Stagehand keeps the first action's result when step
                // two finds nothing.
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
                Err(failure) if failure.stale_ref && stale_retry_left => {
                    stale_retry_left = false;
                    continue;
                }
                Err(failure) => break failure.end,
            }
            actions.push(ActActionReport {
                line:        self.vars.mask(&action.line()),
                description: self.vars.mask(&description),
            });
            match then {
                FollowUp::Replan if first_action.is_none() => {
                    first_action = Some(action.describe_for_model(&description));
                }
                FollowUp::Replan | FollowUp::Done => break StepEnd::Passed,
            }
        };
        let report = ActReport {
            model,
            actions,
            usage: usage_report(model_calls, usage),
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
                end:       line.timed_out(),
                stale_ref: false,
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
                let stale_ref =
                    matches!(&outcome, StepOutcome::ShimError(error) if error.kind == "stale-ref");
                Err(ShimFailure {
                    end: self.apply_outcome(line.node, outcome, state, budget),
                    stale_ref,
                })
            }
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
