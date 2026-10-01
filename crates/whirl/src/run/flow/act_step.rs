//! One `ACT` line (SPEC 7.4): take a snapshot, ask the model, check its
//! answer, and run the chosen action as an ordinary shim command. A
//! two-step action plans once more on a fresh snapshot. The whole line
//! shares one step budget (SPEC 12).

use std::error::Error as _;
use std::time::{Duration, Instant};

use serde_json::Value as Json;
use whirl_ai::{
    ActDecision, ActPlanner, FollowUp, Instruction, ModelError, ModelErrorKind, PageSnapshot,
    PlanError, PlanRequest, PlanStep, PlanUsage, PlannedAction, PlannedBy,
};
use whirl_lang::parse_action_line;
use whirl_report::model::{
    ActActionReport, ActJevUsage, ActReport, ActUsage, StepError, StepWarning,
};
use whirl_shim::{
    AriaSnapshotResult, ReadResult, ShimClient, StepCommand, StepOutcome, StepRequest,
};

use super::{EntryState, FlowExec, StepBudget, StepEnd, StepNode, entry_timeout_error};
use crate::run::cache::{CacheEntry, CachedAction, EntryKind};

/// The budget of one `ACT` line.
#[derive(Clone, Copy, Debug)]
pub(super) struct ActBudget {
    pub(super) timeout_ms:      u64,
    pub(super) entry_capped:    bool,
    pub(super) entry_budget_ms: u64,
}

/// What stays fixed while one `ACT` line, or a line with an `ai:` target,
/// runs its shim calls and model calls.
#[derive(Clone, Copy)]
pub(super) struct ActLine<'a> {
    pub(super) node:        StepNode<'a>,
    pub(super) title:       &'a str,
    pub(super) deadline:    Instant,
    pub(super) budget:      ActBudget,
    /// True until the line's first shim call, which starts the entry's
    /// observation windows when the line is the entry's first step.
    pub(super) entry_start: bool,
    /// What the timeout message names, such as `ACT`.
    pub(super) what:        &'static str,
}

impl ActLine<'_> {
    pub(super) fn remaining_ms(&self) -> u64 {
        u64::try_from(
            self.deadline
                .saturating_duration_since(Instant::now())
                .as_millis(),
        )
        .unwrap_or(u64::MAX)
    }

    /// The failure when the line's budget runs out (SPEC 12).
    pub(super) fn timed_out(&self) -> StepEnd {
        if self.budget.entry_capped {
            return StepEnd::Failed(entry_timeout_error(self.budget.entry_budget_ms));
        }
        StepEnd::Failed(StepError {
            code: "timeout".to_owned(),
            message: format!(
                "timeout: {} did not finish within {}ms",
                self.what, self.budget.timeout_ms
            ),
            ..StepError::default()
        })
    }
}

/// A failed shim call inside an `ACT` line: its classified end, and the
/// kind of the shim's error answer, if the shim answered.
pub(super) struct ShimFailure {
    pub(super) end:        StepEnd,
    pub(super) shim_error: Option<String>,
}

impl ShimFailure {
    /// Whether the call failed only because the page replaced the snapshot
    /// element.
    pub(super) fn stale_ref(&self) -> bool {
        self.shim_error.as_deref() == Some("stale-ref")
    }
}

pub(super) fn act_failure(code: &str, message: &str) -> StepError {
    StepError {
        code: code.to_owned(),
        message: format!("{code}: {message}"),
        ..StepError::default()
    }
}

pub(super) fn warning(code: &str, message: String) -> StepWarning {
    StepWarning {
        code: code.to_owned(),
        message,
    }
}

/// How the replay of one cached line went (SPEC 12.1).
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum Replay {
    Ran,
    /// The elements at these locator positions no longer fit their
    /// fingerprints; nothing ran.
    Missing(Vec<usize>),
    /// The line did not parse or build, or its action failed.
    Failed,
}

/// What one `ACT` line ran and spent, and the lines its cache entry holds.
struct ActRecord {
    model:       String,
    reports:     Vec<ActActionReport>,
    usage:       PlanUsage,
    /// Each action's cache line; `Err` when the cache cannot hold it.
    lines:       Vec<Option<CachedAction>>,
    uncacheable: Option<(&'static str, String)>,
}

impl ActRecord {
    fn new(model: String) -> Self {
        Self {
            model,
            reports: Vec::new(),
            usage: PlanUsage::default(),
            lines: Vec::new(),
            uncacheable: None,
        }
    }

    fn push_line(&mut self, line: Result<CachedAction, (&'static str, String)>) {
        match line {
            Ok(line) => self.lines.push(Some(line)),
            Err(reason) => {
                self.uncacheable.get_or_insert(reason);
                self.lines.push(None);
            }
        }
    }

    /// The lines of the line's cache entry.
    fn entry_lines(&self) -> Result<Vec<CachedAction>, (&'static str, String)> {
        if let Some(reason) = &self.uncacheable {
            return Err(reason.clone());
        }
        Ok(self.lines.iter().flatten().cloned().collect())
    }

    fn report(
        self,
        planner: Option<&dyn ActPlanner>,
        cache: &str,
        cached: Option<Vec<String>>,
    ) -> ActReport {
        let name = planner.map_or(PlannedBy::Llm.as_str(), ActPlanner::name);
        ActReport {
            model: self.model,
            planner: name.to_owned(),
            actions: self.reports,
            usage: usage_report(self.usage, name == PlannedBy::Jev.as_str()),
            cache: Some(cache.to_owned()),
            cached,
        }
    }
}

/// Token usage for the report: cached prompt tokens count as input, and
/// reasoning tokens as output. Jev's usage appears only with `--jev`, and
/// the step's cost includes Jev's.
pub(super) fn usage_report(usage: PlanUsage, jev_planner: bool) -> ActUsage {
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
    /// Runs one `ACT` line and reports what it did, pass or fail, with the
    /// warnings of its AI cache (SPEC 7.4, 12.1).
    pub(super) async fn run_act(
        &mut self,
        node: StepNode<'_>,
        instruction: &Instruction,
        scope: Option<Json>,
        title: &str,
        budget: ActBudget,
        client: &mut ShimClient,
        state: &mut EntryState,
    ) -> (StepEnd, Option<ActReport>, Vec<StepWarning>) {
        let Some(model) = self.options.model.clone() else {
            let error = act_failure("act-model", "ACT needs the model option");
            return (StepEnd::Error(error), None, Vec::new());
        };
        let mut line = ActLine {
            node,
            title,
            deadline: Instant::now() + Duration::from_millis(budget.timeout_ms),
            budget,
            entry_start: state.steps.is_empty(),
            what: "ACT",
        };
        let key = self
            .cache
            .key(EntryKind::Act, node.line(), node.raw_text(), None);
        let mut record = ActRecord::new(model.clone());

        // Replay the cached lines; the first that misses heals (SPEC 12.1).
        let cached = match self.cache.get(&key) {
            Some(CacheEntry::Act { actions, .. }) => Some(actions.clone()),
            _ => None,
        };
        let mut first_action = None;
        if let Some(cached) = &cached {
            let mut hit = true;
            for action in cached {
                if self
                    .replay_action(&mut line, action, None, client, state)
                    .await
                    != Replay::Ran
                {
                    hit = false;
                    break;
                }
                record.reports.push(ActActionReport {
                    line:        self.vars.mask(&action.line),
                    description: "the cached line".to_owned(),
                    planned_by:  "cache".to_owned(),
                    error:       None,
                });
                record.lines.push(Some(action.clone()));
                first_action = Some(format!("the Whirl line {}", action.line));
            }
            if hit {
                self.cache.keep(node.line(), CacheEntry::Act {
                    line:       key.line,
                    occurrence: key.occurrence,
                    model:      model.clone(),
                    actions:    cached.clone(),
                });
                let report = record.report(self.run.planner, "hit", None);
                return (StepEnd::Passed, Some(report), Vec::new());
            }
            // The page is not what the cache saw: plan from the page as it
            // is now.
            if record.lines.is_empty() {
                first_action = None;
            }
        }
        let status = if cached.is_some() { "healed" } else { "miss" };
        let cached_lines = cached.as_ref().map(|actions| {
            actions
                .iter()
                .map(|action| self.vars.mask(&action.line))
                .collect()
        });
        if !self.cache.mode().allows_model() {
            let report = record.report(self.run.planner, status, cached_lines);
            return (
                super::ai_step::cache_miss(&format!("ACT line {}", node.line())),
                Some(report),
                Vec::new(),
            );
        }
        let Some(planner) = self.run.planner else {
            let error = act_failure("act-model", "ACT needs the model option and a planner");
            return (StepEnd::Error(error), None, Vec::new());
        };

        // A chosen action that fails heals once: a new snapshot and one more
        // plan (SPEC 7.4).
        let mut heal_left = true;
        let end = loop {
            let snapshot = match self
                .act_shim_call(
                    &mut line,
                    StepCommand::AriaSnapshot {
                        locator: scope.clone(),
                        settle:  true,
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
            record.usage = record.usage.saturating_add(plan.usage);
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

            // The line the cache writes, from the generator's locators,
            // before the action changes the page (SPEC 12.1).
            let cache_line = self
                .cache_line(&mut line, &action, instruction, client, state)
                .await;

            // With a heal left, the action gets half of the line's time,
            // so the heal has time to run (SPEC 7.4).
            let mut attempt = if heal_left {
                ActLine {
                    deadline: Instant::now() + Duration::from_millis(line.remaining_ms() / 2),
                    ..line
                }
            } else {
                line
            };
            let result = self
                .act_shim_call(&mut attempt, action.command(instruction), client, state)
                .await;
            line.entry_start = attempt.entry_start;
            match result {
                Ok(_) => {}
                Err(ShimFailure {
                    end: StepEnd::Failed(_),
                    ..
                }) if heal_left => {
                    heal_left = false;
                    continue;
                }
                Err(failure) => break failure.end,
            }
            record.reports.push(ActActionReport {
                line:        self.vars.mask(&action.line()),
                description: self.vars.mask(&description),
                planned_by:  planned_by.as_str().to_owned(),
                error:       None,
            });
            record.push_line(cache_line);
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
        let mut warnings = Vec::new();
        if matches!(end, StepEnd::Passed) {
            match record.entry_lines() {
                Ok(actions) => {
                    self.cache.keep(node.line(), CacheEntry::Act {
                        line: key.line,
                        occurrence: key.occurrence,
                        model: model.clone(),
                        actions,
                    });
                    warnings.push(if cached.is_some() {
                        warning(
                            "healed",
                            format!(
                                "the cached lines of ACT line {} no longer fit; the model planned the rest",
                                node.line()
                            ),
                        )
                    } else {
                        warning(
                            "cache-miss",
                            format!("ACT line {} has no cache entry", node.line()),
                        )
                    });
                }
                Err((code, reason)) => warnings.push(warning(
                    code,
                    format!(
                        "ACT line {} is not cached, so the model plans it on every run: {reason}",
                        node.line()
                    ),
                )),
            }
        }
        let report = record.report(Some(planner), status, cached_lines);
        (end, Some(report), warnings)
    }

    /// Runs one cached line after its fingerprint check, with at most half
    /// of the line's remaining time (SPEC 12.1). `cap_ms`, when given,
    /// stands in for the remaining time when it is shorter. False is a
    /// miss.
    pub(super) async fn replay_action(
        &mut self,
        line: &mut ActLine<'_>,
        cached: &CachedAction,
        cap_ms: Option<u64>,
        client: &mut ShimClient,
        state: &mut EntryState,
    ) -> Replay {
        let Ok(action) = parse_action_line(&cached.line) else {
            return Replay::Failed;
        };
        let locators = action.kind.locators();
        if locators.len() != cached.fingerprints.len() {
            return Replay::Failed;
        }
        let capped = ActLine {
            deadline: cap_ms.map_or(line.deadline, |cap| {
                line.deadline
                    .min(Instant::now() + Duration::from_millis(cap))
            }),
            ..*line
        };
        let mut missing = Vec::new();
        for (index, (locator, fingerprint)) in
            locators.into_iter().zip(&cached.fingerprints).enumerate()
        {
            if self
                .check_cached(&capped, locator, fingerprint, client, state)
                .await
                .is_none()
            {
                missing.push(index);
            }
        }
        if !missing.is_empty() {
            return Replay::Missing(missing);
        }
        let Ok(command) = self.build_action(&action) else {
            return Replay::Failed;
        };
        let time_ms = capped.remaining_ms() / 2;
        let mut attempt = ActLine {
            deadline: Instant::now() + Duration::from_millis(time_ms),
            ..*line
        };
        let ran = self
            .act_shim_call(&mut attempt, command, client, state)
            .await
            .is_ok();
        line.entry_start = attempt.entry_start;
        if ran { Replay::Ran } else { Replay::Failed }
    }

    /// The cache line of a planned action, with its fingerprints, or the
    /// warning code and reason when the cache cannot hold it (SPEC 12.1).
    pub(super) async fn cache_line(
        &mut self,
        line: &mut ActLine<'_>,
        action: &PlannedAction,
        instruction: &Instruction,
        client: &mut ShimClient,
        state: &mut EntryState,
    ) -> Result<CachedAction, (&'static str, String)> {
        let mut locators = Vec::new();
        let mut fingerprints = Vec::new();
        for target in action.targets() {
            locators.push(self.generated_locator(line, target, client, state).await?);
            fingerprints.push(target.fingerprint());
        }
        let text = action
            .cache_line(&locators, |text| instruction.cache_value(text))
            .ok_or((
                "cache-secret",
                "an argument holds a masked value that no variable reference names".to_owned(),
            ))?;
        Ok(CachedAction {
            line: text,
            fingerprints,
        })
    }

    /// Runs one shim command inside the line's remaining budget. A
    /// failure is classified like any step's (protocol section 7).
    pub(super) async fn act_shim_call(
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
            // A read inside a check's own trace group has no title.
            title: (!line.title.is_empty()).then(|| line.title.to_owned()),
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
    pub(super) async fn read_back_value(
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
    pub(super) fn model_failure(&self, error: &ModelError, line: &ActLine<'_>) -> StepEnd {
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
            ModelErrorKind::ContentFilter | ModelErrorKind::ContextLength => {
                StepEnd::Failed(act_failure("act-model", &message))
            }
            ModelErrorKind::ResponseDecode => {
                StepEnd::Failed(act_failure("act-invalid-decision", &message))
            }
            _ => StepEnd::Error(act_failure("act-model", &message)),
        }
    }
}
