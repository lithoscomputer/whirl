//! One `GOAL` line (SPEC 7.7): replay the cached path, or ask the model
//! for one action at a time on a fresh snapshot until it answers `done` or
//! `impossible`, the actions run out, or the time does. A cached path that
//! misses heals from the current page.

use std::time::{Duration, Instant};

use whirl_ai::{
    ActDecision, GoalStatus, Instruction, ModelClient, PageSnapshot, PlanUsage, PlannedAction,
    Target, goal_message, target_message,
};
use whirl_lang::{parse_action_line, render_action};
use whirl_report::model::{ActActionReport, GoalReport, StepError, StepWarning};
use whirl_shim::{AriaSnapshotResult, ShimClient, StepCommand};

use super::act_step::{
    ActBudget, ActLine, Replay, ShimFailure, act_failure, usage_report, warning,
};
use super::{EntryState, FlowExec, StepEnd, StepNode};
use crate::cache::{CacheEntry, CachedAction, EntryKind};

/// The most actions one `GOAL` line runs (SPEC 7.7).
const MAX_ACTIONS: usize = 20;

/// A `GOAL` line's time when it has no `@duration` (SPEC 7.7).
pub(super) const DEFAULT_TIMEOUT_MS: u64 = 120_000;

/// A `GOAL` line ready to run.
pub(super) struct GoalPlan {
    /// The goal, with masked values as placeholders.
    pub(super) goal: Instruction,
}

/// What one `GOAL` line ran and spent, and the lines its cache entry
/// holds.
struct GoalRecord {
    model:       String,
    reports:     Vec<ActActionReport>,
    usage:       PlanUsage,
    /// The actions that ran, as the cache holds them.
    lines:       Vec<CachedAction>,
    uncacheable: Option<(&'static str, String)>,
    /// The steps so far, as the model sees them.
    history:     Vec<String>,
    /// `done` or `impossible`, when the model ended the goal.
    end:         Option<&'static str>,
    reason:      Option<String>,
}

impl GoalRecord {
    fn report(self, cache: &str, cached: Option<Vec<String>>) -> GoalReport {
        GoalReport {
            model: self.model,
            actions: self.reports,
            end: self.end.map(str::to_owned),
            reason: self.reason,
            usage: usage_report(self.usage, false),
            cache: cache.to_owned(),
            cached,
        }
    }
}

/// Renames a model failure's code under `ACT`'s rules to `goal-model`
/// (SPEC 7.7). A malformed answer keeps `act-invalid-decision`.
fn recode(end: StepEnd) -> StepEnd {
    let rename = |mut error: StepError| {
        if error.code == "act-model" {
            error.message = error.message.replacen("act-model", "goal-model", 1);
            "goal-model".clone_into(&mut error.code);
        }
        error
    };
    match end {
        StepEnd::Failed(error) => StepEnd::Failed(rename(error)),
        StepEnd::Error(error) => StepEnd::Error(rename(error)),
        other @ StepEnd::Passed => other,
    }
}

impl FlowExec<'_> {
    /// Finds again the elements of a cached line that no longer fit their
    /// fingerprints, from their role and name, and runs the line on them
    /// with its own method and arguments (SPEC 12.1). `None` when the model
    /// finds none or several, or the line fails.
    #[expect(
        clippy::too_many_arguments,
        reason = "the re-find needs the line, the cached action, and the flow's state"
    )]
    async fn refind(
        &mut self,
        line: &mut ActLine<'_>,
        cached: &CachedAction,
        missing: &[usize],
        action_cap: u64,
        record: &mut GoalRecord,
        client: &mut ShimClient,
        state: &mut EntryState,
    ) -> Option<(CachedAction, String)> {
        let (Some(model_client), Some(model)) = (self.run.model, self.options.model.clone()) else {
            return None;
        };
        let mut action = parse_action_line(&cached.line).ok()?;
        let mut fingerprints = cached.fingerprints.clone();
        let mut descriptions = Vec::new();
        for &index in missing {
            let fingerprint = fingerprints.get(index)?.clone();
            let result = self
                .act_shim_call(
                    line,
                    StepCommand::AriaSnapshot {
                        locator: None,
                        settle:  true,
                    },
                    client,
                    state,
                )
                .await
                .ok()?;
            let snapshot = serde_json::from_value::<AriaSnapshotResult>(result).ok()?;
            let snapshot = PageSnapshot::parse(&snapshot.snapshot).of_page();
            let wanted = match &fingerprint.name {
                Some(name) => format!(
                    "the {} named \"{name}\", which the step `{}` acts on; the page may show it with another name now",
                    fingerprint.role, cached.line
                ),
                None => format!(
                    "the {}, which the step `{}` acts on",
                    fingerprint.role, cached.line
                ),
            };
            let user = target_message(&wanted, &[], snapshot.text());
            let reply = model_client
                .find_elements(&model, &user, line.deadline)
                .await;
            record.usage.model_calls = record.usage.model_calls.saturating_add(1);
            let reply = reply.ok()?;
            record.usage.model = record.usage.model.saturating_add(reply.usage);
            let mut found: Vec<(Target, String)> = Vec::new();
            for element in reply.answer.ok()?.elements {
                let target = snapshot.target(&element.element_id)?;
                if !found.iter().any(|(known, _)| known == &target) {
                    found.push((target, element.description));
                }
            }
            let [(target, description)] = <[_; 1]>::try_from(found).ok()?;
            let text = self
                .generated_locator(line, &target, client, state)
                .await
                .ok()?;
            let locator = parse_action_line(&format!("CLICK {text}"))
                .ok()?
                .kind
                .locators()
                .first()
                .map(|locator| (*locator).clone())?;
            **action.kind.locators_mut().get_mut(index)? = locator;
            fingerprints[index] = target.fingerprint();
            descriptions.push(self.vars.mask(&description));
        }
        let command = self.build_action(&action).ok()?;
        let mut attempt = ActLine {
            deadline: Instant::now() + Duration::from_millis(line.remaining_ms().min(action_cap)),
            ..*line
        };
        let ran = self
            .act_shim_call(&mut attempt, command, client, state)
            .await;
        line.entry_start = attempt.entry_start;
        ran.ok()?;
        let refound = CachedAction {
            line: render_action(&action),
            fingerprints,
        };
        Some((refound, descriptions.join("; ")))
    }

    /// Runs one `GOAL` line and reports what it did, pass or fail, with the
    /// warnings of its AI cache (SPEC 7.7, 12.1).
    pub(super) async fn run_goal(
        &mut self,
        node: StepNode<'_>,
        plan: GoalPlan,
        title: &str,
        budget: ActBudget,
        client: &mut ShimClient,
        state: &mut EntryState,
    ) -> (StepEnd, Option<GoalReport>, Vec<StepWarning>) {
        let (Some(model_client), Some(model)) = (self.run.model, self.options.model.clone()) else {
            let error = act_failure("goal-model", "GOAL needs the model option");
            return (StepEnd::Error(error), None, Vec::new());
        };
        let mut line = ActLine {
            node,
            title,
            deadline: Instant::now() + Duration::from_millis(budget.timeout_ms),
            budget,
            entry_start: state.steps.is_empty(),
            what: "GOAL",
        };
        let key = self
            .cache
            .key(EntryKind::Goal, node.line(), node.raw_text(), None);
        let mut record = GoalRecord {
            model:       model.clone(),
            reports:     Vec::new(),
            usage:       PlanUsage::default(),
            lines:       Vec::new(),
            uncacheable: None,
            history:     Vec::new(),
            end:         None,
            reason:      None,
        };
        let action_cap = self.options.step_timeout_ms;

        // Replay the cached path; the first line that misses heals from
        // the current page (SPEC 12.1).
        let cached = match self.cache.get(&key) {
            Some(CacheEntry::Goal { actions, .. }) => Some(actions.clone()),
            _ => None,
        };
        if let Some(cached) = &cached {
            let mut hit = true;
            let mut refound = false;
            for action in cached {
                let replay = self
                    .replay_action(&mut line, action, Some(action_cap), client, state)
                    .await;
                // A line whose element changed its role or name asks the
                // model to find that element again, and the path goes on.
                let ran = match replay {
                    Replay::Ran => Some((action.clone(), "the cached line".to_owned())),
                    Replay::Missing(missing) if self.cache.mode().allows_model() => {
                        self.refind(
                            &mut line,
                            action,
                            &missing,
                            action_cap,
                            &mut record,
                            client,
                            state,
                        )
                        .await
                    }
                    Replay::Missing(_) | Replay::Failed => None,
                };
                let Some((ran, description)) = ran else {
                    hit = false;
                    break;
                };
                let text = self.vars.mask(&ran.line);
                let planned_by = if &ran == action { "cache" } else { "llm" };
                refound |= planned_by == "llm";
                record.reports.push(ActActionReport {
                    line: text.clone(),
                    description,
                    planned_by: planned_by.to_owned(),
                    error: None,
                });
                record.history.push(text);
                record.lines.push(ran);
            }
            if hit && !refound {
                self.cache.keep(node.line(), CacheEntry::Goal {
                    line:       key.line,
                    occurrence: key.occurrence,
                    model:      model.clone(),
                    actions:    cached.clone(),
                });
                return (
                    StepEnd::Passed,
                    Some(record.report("hit", None)),
                    Vec::new(),
                );
            }
            // After a re-find, the model sees every line that ran and says
            // whether the goal is done (SPEC 7.7).
        }
        let status = if cached.is_some() { "healed" } else { "miss" };
        let cached_lines = cached.as_ref().map(|actions| {
            actions
                .iter()
                .map(|action| self.vars.mask(&action.line))
                .collect()
        });
        if !self.cache.mode().allows_model() {
            return (
                super::ai_step::cache_miss(&format!("GOAL line {}", node.line())),
                Some(record.report(status, cached_lines)),
                Vec::new(),
            );
        }

        let end = self
            .plan_goal(
                &mut line,
                &plan.goal,
                &model,
                model_client,
                action_cap,
                &mut record,
                client,
                state,
            )
            .await;
        let mut warnings = Vec::new();
        if matches!(end, StepEnd::Passed) {
            match record.uncacheable.clone() {
                None => {
                    self.cache.keep(node.line(), CacheEntry::Goal {
                        line:       key.line,
                        occurrence: key.occurrence,
                        model:      model.clone(),
                        actions:    record.lines.clone(),
                    });
                    warnings.push(if cached.is_some() {
                        warning(
                            "healed",
                            format!(
                                "the cached lines of GOAL line {} no longer fit; the model planned the rest",
                                node.line()
                            ),
                        )
                    } else {
                        warning(
                            "cache-miss",
                            format!("GOAL line {} has no cache entry", node.line()),
                        )
                    });
                }
                Some((code, reason)) => warnings.push(warning(
                    code,
                    format!(
                        "GOAL line {} is not cached, so the model plans it on every run: {reason}",
                        node.line()
                    ),
                )),
            }
        }
        (end, Some(record.report(status, cached_lines)), warnings)
    }

    /// Asks the model for one action at a time and runs it, until the goal
    /// ends (SPEC 7.7).
    #[expect(
        clippy::too_many_arguments,
        reason = "the loop needs the line, the goal, the model, and the flow's state"
    )]
    async fn plan_goal(
        &mut self,
        line: &mut ActLine<'_>,
        goal: &Instruction,
        model: &str,
        model_client: &ModelClient,
        action_cap: u64,
        record: &mut GoalRecord,
        client: &mut ShimClient,
        state: &mut EntryState,
    ) -> StepEnd {
        let placeholders = goal.bindings().placeholders();
        loop {
            let result = match self
                .act_shim_call(
                    line,
                    StepCommand::AriaSnapshot {
                        locator: None,
                        settle:  true,
                    },
                    client,
                    state,
                )
                .await
            {
                Ok(result) => result,
                Err(failure) => return failure.end,
            };
            let Ok(result) = serde_json::from_value::<AriaSnapshotResult>(result) else {
                return StepEnd::Error(act_failure(
                    "internal",
                    "malformed ariaSnapshot result from the shim",
                ));
            };
            let snapshot = PageSnapshot::parse(&result.snapshot).of_page();
            let user = goal_message(
                goal.prompt(),
                &placeholders,
                &record.history,
                snapshot.text(),
            );
            let reply = model_client.goal_step(model, &user, line.deadline).await;
            record.usage.model_calls = record.usage.model_calls.saturating_add(1);
            let reply = match reply {
                Ok(reply) => reply,
                Err(error) => return recode(self.model_failure(&error, line)),
            };
            record.usage.model = record.usage.model.saturating_add(reply.usage);
            let answer = match reply.answer {
                Ok(answer) => answer,
                Err(error) => {
                    return StepEnd::Failed(act_failure(
                        "act-invalid-decision",
                        &format!("the model's answer does not match the GOAL schema: {error}"),
                    ));
                }
            };
            let reason = self.vars.mask(&answer.reason);
            record.reason = Some(reason.clone());
            match answer.status {
                GoalStatus::Done => {
                    record.end = Some("done");
                    return StepEnd::Passed;
                }
                GoalStatus::Impossible => {
                    record.end = Some("impossible");
                    return StepEnd::Failed(StepError {
                        code:       "goal-impossible".to_owned(),
                        message:    format!("goal-impossible: {reason}"),
                        expected:   Some(self.vars.mask(goal.prompt())),
                        actual:     Some(reason),
                        candidates: None,
                    });
                }
                GoalStatus::Act if record.reports.len() >= MAX_ACTIONS => {
                    return StepEnd::Failed(act_failure(
                        "goal-limit",
                        &format!(
                            "the goal was not done after {MAX_ACTIONS} actions; the model's last reason: {reason}"
                        ),
                    ));
                }
                GoalStatus::Act => {}
            }
            let mut planned = Vec::new();
            for inference in answer.into_acts() {
                match snapshot.decide(inference, goal) {
                    Ok(ActDecision::Perform {
                        action,
                        description,
                        ..
                    }) => planned.push((action, description)),
                    Ok(ActDecision::NoMatch) => {}
                    Err(error) => {
                        return StepEnd::Failed(act_failure(
                            "act-invalid-decision",
                            &self.vars.mask(&error.to_string()),
                        ));
                    }
                }
            }
            if planned.is_empty() {
                return StepEnd::Failed(act_failure(
                    "act-invalid-decision",
                    "the model answered act without an action",
                ));
            }
            // The actions of one answer, such as the fields of a form, run in
            // order until one fails (SPEC 7.7).
            for (action, description) in planned {
                if record.reports.len() >= MAX_ACTIONS {
                    break;
                }
                match self
                    .run_goal_action(
                        line,
                        &action,
                        &description,
                        goal,
                        action_cap,
                        record,
                        client,
                        state,
                    )
                    .await
                {
                    Ok(true) => {}
                    Ok(false) => break,
                    Err(end) => return end,
                }
            }
        }
    }

    /// Runs one action that the model planned for a goal and records it.
    /// `Ok(false)` when the action failed and the model plans again; `Err`
    /// when the step ends.
    #[expect(
        clippy::too_many_arguments,
        reason = "the action needs the line, the goal, and the flow's state"
    )]
    async fn run_goal_action(
        &mut self,
        line: &mut ActLine<'_>,
        action: &PlannedAction,
        description: &str,
        goal: &Instruction,
        action_cap: u64,
        record: &mut GoalRecord,
        client: &mut ShimClient,
        state: &mut EntryState,
    ) -> Result<bool, StepEnd> {
        // The line the cache writes, before the action changes the page
        // (SPEC 12.1).
        let cache_line = self.cache_line(line, action, goal, client, state).await;
        let mut attempt = ActLine {
            deadline: Instant::now() + Duration::from_millis(line.remaining_ms().min(action_cap)),
            ..*line
        };
        let ran = self
            .act_shim_call(&mut attempt, action.command(goal), client, state)
            .await;
        line.entry_start = attempt.entry_start;
        let mut failure = match ran {
            Ok(_) => None,
            Err(ShimFailure {
                end: StepEnd::Failed(error),
                ..
            }) => Some(error.message),
            Err(failure) => return Err(failure.end),
        };
        if failure.is_none()
            && let Some(read_back) = action.fill_read_back(goal)
        {
            match self
                .read_back_value(line, read_back.command.clone(), client, state)
                .await
            {
                Ok(Some(held)) if !read_back.matches(&held) => {
                    failure = Some(format!(
                        "act-fill-mismatch: {}",
                        read_back.mismatch(action, &held)
                    ));
                }
                Ok(_) => {}
                Err(failure) => return Err(failure.end),
            }
        }
        if line.remaining_ms() == 0 {
            return Err(line.timed_out());
        }
        let text = self.vars.mask(&action.line());
        let error = failure.map(|message| self.vars.mask(&message));
        record.history.push(match &error {
            Some(error) => format!("{text} (failed: {error})"),
            None => text.clone(),
        });
        record.reports.push(ActActionReport {
            line:        text,
            description: self.vars.mask(description),
            planned_by:  "llm".to_owned(),
            error:       error.clone(),
        });
        if error.is_some() {
            return Ok(false);
        }
        match cache_line {
            Ok(cached) => record.lines.push(cached),
            Err(reason) => {
                record.uncacheable.get_or_insert(reason);
            }
        }
        Ok(true)
    }
}
