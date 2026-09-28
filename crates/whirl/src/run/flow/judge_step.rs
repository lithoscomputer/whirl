//! One `JUDGE` line (SPEC 9.8): take the AI snapshot and a settled
//! screenshot of the page or the element, ask the model whether the claim
//! holds, and pass on `yes`, fail on `no`, and warn on `unsure`.

use std::time::{Duration, Instant};

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use serde::Deserialize;
use serde_json::Value as Json;

use super::act_step::{ActBudget, ActLine, act_failure, usage_report};
use super::{EntryState, FlowExec, StepEnd, StepNode};
use crate::report::model::{JudgeReport, StepError, StepWarning};
use crate::run::act::{Instruction, PageSnapshot, PlanUsage, Verdict, judge_message};
use crate::run::shim::{AriaSnapshotResult, ShimClient, StepCommand};

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

        let placeholders = plan.claim.bindings().placeholders();
        let text = judge_message(plan.claim.prompt(), &placeholders, snapshot.text());
        let reply = model_client.judge(&model, &text, &png, line.deadline).await;
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
        let answer = match reply.answer {
            Ok(answer) => answer,
            Err(error) => {
                let error = act_failure(
                    "judge-model",
                    &format!("the model's answer does not match the JUDGE schema: {error}"),
                );
                return (StepEnd::Failed(error), Some(report), Vec::new());
            }
        };
        let reason = self.vars.mask(&answer.reason);
        report.verdict = Some(answer.verdict.as_str().to_owned());
        report.reason = Some(reason.clone());
        match answer.verdict {
            Verdict::Yes => (StepEnd::Passed, Some(report), Vec::new()),
            Verdict::No => {
                let error = StepError {
                    code:       "judge-false".to_owned(),
                    message:    format!("judge-false: {reason}"),
                    expected:   Some(self.vars.mask(plan.claim.prompt())),
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
                        self.vars.mask(plan.claim.prompt())
                    ),
                };
                (StepEnd::Passed, Some(report), vec![warning])
            }
        }
    }
}
