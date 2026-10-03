//! One `JUDGE` line (SPEC 9.8): take the AI snapshot and a settled
//! screenshot of the page or the element, ask the model whether the claim
//! holds, and pass on `yes`, fail on `no`, and warn on `unsure`.

use std::collections::HashMap;
use std::time::{Duration, Instant};
use std::{iter, mem};

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use serde::Deserialize;
use serde_json::Value as Json;
use whirl_ai::{Instruction, JudgeAnswer, PageSnapshot, PlanUsage, Verdict, judge_message};
use whirl_lang::ast::{self, CheckStep};
use whirl_lang::render_snapshot_target;
use whirl_report::model::{JudgeReport, StepError, StepWarning};
use whirl_shim::{AriaSnapshotResult, ShimClient, StepCommand};

use super::act_step::{ActBudget, ActLine, act_failure, usage_report};
use super::{EntryState, FlowExec, StepEnd, StepNode};

/// A `JUDGE` line ready to run.
pub(super) struct JudgePlan {
    /// The claim, with masked values as placeholders.
    pub(super) claim: Instruction,
    /// The wire locator of the element the model sees.
    pub(super) scope: Option<Json>,
}

/// `judgeScreenshot` result (protocol 4.9).
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ScreenshotResult {
    png_base64: String,
}

/// The entry's `JUDGE` batches: runs of consecutive `JUDGE` lines with the
/// same scope and timeout, keyed by the line of the first. One model call
/// judges every claim of a batch on one screenshot (SPEC 9.8).
pub(super) fn batches(entry: &ast::Entry) -> HashMap<u32, Vec<ast::Judge>> {
    let key = |judge: &ast::Judge| {
        (
            judge.scope.as_ref().map(render_snapshot_target),
            judge.timeout.map(ast::DurationLit::millis),
        )
    };
    let mut batches = HashMap::new();
    let mut run: Vec<ast::Judge> = Vec::new();
    let mut close = |run: &mut Vec<ast::Judge>| {
        if run.len() > 1 {
            batches.insert(run[0].line, mem::take(run));
        }
        run.clear();
    };
    for check in &entry.checks {
        match check {
            CheckStep::Judge(judge) => {
                if run.last().is_some_and(|last| key(last) != key(judge)) {
                    close(&mut run);
                }
                run.push(judge.clone());
            }
            CheckStep::Assert(_) | CheckStep::Capture(_) => close(&mut run),
        }
    }
    close(&mut run);
    batches
}

/// Renames the codes of a model failure under `ACT`'s rules to
/// `judge-model` (SPEC 9.8).
fn recode(end: StepEnd) -> StepEnd {
    let rename = |mut error: StepError| {
        if matches!(error.code.as_str(), "act-model" | "act-invalid-decision") {
            error.message = error.message.replacen(&error.code, "judge-model", 1);
            "judge-model".clone_into(&mut error.code);
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
    /// Runs one `JUDGE` line and reports the verdict, pass or fail, with
    /// the `judge-unsure` warning of an `unsure` answer.
    pub(super) async fn run_judge(
        &mut self,
        node: StepNode<'_>,
        plan: JudgePlan,
        title: &str,
        budget: ActBudget,
        client: &mut ShimClient,
        state: &mut EntryState,
    ) -> (StepEnd, Option<JudgeReport>, Vec<StepWarning>) {
        let (Some(model_client), Some(model)) = (self.run.model, self.options.model.clone()) else {
            let error = act_failure("judge-model", "JUDGE needs the model option");
            return (StepEnd::Error(error), None, Vec::new());
        };
        let mut line = ActLine {
            node,
            title,
            deadline: Instant::now() + Duration::from_millis(budget.timeout_ms),
            budget,
            entry_start: state.steps.is_empty(),
            what: "JUDGE",
        };
        let mut report = JudgeReport {
            model:   model.clone(),
            verdict: None,
            reason:  None,
            usage:   usage_report(PlanUsage::default(), false),
        };
        // An earlier line of the batch already judged this claim.
        if let Some(answer) = self.judge_answers.remove(&node.line()) {
            return self.judge_verdict(&answer, &plan.claim, report);
        }

        let snapshot = match self
            .act_shim_call(
                &mut line,
                StepCommand::AriaSnapshot {
                    locator: plan.scope.clone(),
                    settle:  true,
                },
                client,
                state,
            )
            .await
        {
            Ok(result) => match serde_json::from_value::<AriaSnapshotResult>(result) {
                Ok(result) if plan.scope.is_none() => {
                    PageSnapshot::parse(&result.snapshot).of_page()
                }
                Ok(result) => PageSnapshot::parse(&result.snapshot),
                Err(_) => {
                    let error =
                        act_failure("internal", "malformed ariaSnapshot result from the shim");
                    return (StepEnd::Error(error), Some(report), Vec::new());
                }
            },
            Err(failure) => return (failure.end, Some(report), Vec::new()),
        };

        // The frames settle within half of the time left, so the model
        // call has the rest (SPEC 9.8).
        let mut capture = ActLine {
            deadline: Instant::now() + Duration::from_millis(line.remaining_ms() / 2),
            ..line
        };
        let result = match self
            .act_shim_call(
                &mut capture,
                StepCommand::JudgeScreenshot {
                    locator: plan.scope.clone(),
                },
                client,
                state,
            )
            .await
        {
            Ok(result) => result,
            Err(failure) => return (failure.end, Some(report), Vec::new()),
        };
        let Some(png) = serde_json::from_value::<ScreenshotResult>(result)
            .ok()
            .and_then(|result| STANDARD.decode(result.png_base64).ok())
        else {
            let error = act_failure("internal", "malformed judgeScreenshot result from the shim");
            return (StepEnd::Error(error), Some(report), Vec::new());
        };
        line.entry_start = capture.entry_start;

        // Later lines of a batch are judged in the same call (SPEC 9.8).
        let mut batch: Vec<(u32, Instruction)> = Vec::new();
        if let Some(judges) = self.judge_batches.get(&node.line()).cloned() {
            for judge in judges.iter().skip(1) {
                let Ok(claim) = Instruction::try_new(&judge.claim, &mut self.vars) else {
                    batch.clear();
                    break;
                };
                batch.push((judge.line, claim));
            }
        }
        let mut placeholders = plan.claim.bindings().placeholders();
        for (_, claim) in &batch {
            for placeholder in claim.bindings().placeholders() {
                if !placeholders.contains(&placeholder) {
                    placeholders.push(placeholder);
                }
            }
        }
        let claims: Vec<&str> = iter::once(plan.claim.prompt())
            .chain(batch.iter().map(|(_, claim)| claim.prompt()))
            .collect();
        let text = judge_message(&claims, &placeholders, snapshot.text());
        let reply = model_client
            .judge(&model, &text, &png, claims.len(), line.deadline)
            .await;
        let reply = match reply {
            Ok(reply) => reply,
            Err(error) => {
                report.usage = usage_report(
                    PlanUsage {
                        model_calls: 1,
                        ..PlanUsage::default()
                    },
                    false,
                );
                return (
                    recode(self.model_failure(&error, &line)),
                    Some(report),
                    Vec::new(),
                );
            }
        };
        report.usage = usage_report(
            PlanUsage {
                model_calls: 1,
                model: reply.usage,
                ..PlanUsage::default()
            },
            false,
        );
        let mut answers = match reply.answer {
            Ok(answers) if answers.len() == claims.len() => answers.into_iter(),
            Ok(answers) => {
                let error = act_failure(
                    "judge-model",
                    &format!(
                        "the model gave {} verdicts for {} claims",
                        answers.len(),
                        claims.len()
                    ),
                );
                return (StepEnd::Failed(error), Some(report), Vec::new());
            }
            Err(error) => {
                let error = act_failure(
                    "judge-model",
                    &format!("the model's answer does not match the JUDGE schema: {error}"),
                );
                return (StepEnd::Failed(error), Some(report), Vec::new());
            }
        };
        let Some(answer) = answers.next() else {
            unreachable!("one answer for each claim, and there is at least one claim");
        };
        for ((line, _), answer) in batch.iter().zip(answers) {
            self.judge_answers.insert(*line, answer);
        }
        self.judge_verdict(&answer, &plan.claim, report)
    }

    /// The step's end for one answer: `yes` passes, `no` fails with
    /// `judge-false`, and `unsure` passes with `judge-unsure` (SPEC 9.8).
    fn judge_verdict(
        &self,
        answer: &JudgeAnswer,
        claim: &Instruction,
        mut report: JudgeReport,
    ) -> (StepEnd, Option<JudgeReport>, Vec<StepWarning>) {
        let reason = self.vars.mask(&answer.reason);
        report.verdict = Some(answer.verdict.as_str().to_owned());
        report.reason = Some(reason.clone());
        match answer.verdict {
            Verdict::Yes => (StepEnd::Passed, Some(report), Vec::new()),
            Verdict::No => {
                let error = StepError {
                    code:       "judge-false".to_owned(),
                    message:    format!("judge-false: {reason}"),
                    expected:   Some(self.vars.mask(claim.prompt())),
                    actual:     Some(reason),
                    candidates: None,
                };
                (StepEnd::Failed(error), Some(report), Vec::new())
            }
            Verdict::Unsure => {
                let warning = StepWarning {
                    code:    "judge-unsure".to_owned(),
                    message: format!(
                        "the model is unsure whether \"{}\" holds: {reason}",
                        self.vars.mask(claim.prompt())
                    ),
                };
                (StepEnd::Passed, Some(report), vec![warning])
            }
        }
    }
}
