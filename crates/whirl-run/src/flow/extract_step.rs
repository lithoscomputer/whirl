//! One `EXTRACT` line (SPEC 7.6): take the snapshot of the page or the
//! scope, ask the model for a value in the shape of the schema, check the
//! answer, turn link refs into URLs, and keep the value for
//! `extract:NAME` subjects.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use serde_json::Value as Json;
use whirl_ai::{Instruction, ModelClient, PageSnapshot, PlanUsage, extract_message};
use whirl_check::parse_json;
use whirl_report::model::{CaptureValue, ExtractReport, StepError};
use whirl_shim::{AriaSnapshotResult, Locator, ReadResult, ReadSubject, ShimClient, StepCommand};
use whirl_types::Value;

use super::act_step::{ActBudget, ActLine, act_failure, usage_report};
use super::{EntryState, FlowExec, StepEnd, StepNode};
use crate::extract;
use crate::vars::MASK;

/// An `EXTRACT` line ready to run.
pub(super) struct ExtractPlan {
    pub(super) name:        String,
    pub(super) instruction: Instruction,
    /// The element the snapshot is limited to.
    pub(super) scope:       Option<Locator>,
    pub(super) schema:      Option<Json>,
}

/// The values `EXTRACT` lines read, by name; `None` when the model found
/// no value.
pub(super) type ExtractValues = HashMap<String, Option<Value>>;

fn failure(code: &str, message: &str) -> StepEnd {
    StepEnd::Failed(StepError {
        code: code.to_owned(),
        message: format!("{code}: {message}"),
        ..StepError::default()
    })
}

/// A model failure under the rules of `ACT`, with `EXTRACT`'s codes (SPEC
/// 7.6): `extract-model`, and `extract-schema` for an answer that is not
/// JSON.
fn recode(end: StepEnd) -> StepEnd {
    let rename = |mut error: StepError| {
        let code = match error.code.as_str() {
            "act-model" => "extract-model",
            "act-invalid-decision" => "extract-schema",
            _ => return error,
        };
        error.message = error.message.replacen(&error.code, code, 1);
        code.clone_into(&mut error.code);
        error
    };
    match end {
        StepEnd::Failed(error) => StepEnd::Failed(rename(error)),
        StepEnd::Error(error) => StepEnd::Error(rename(error)),
        other @ StepEnd::Passed => other,
    }
}

impl FlowExec<'_> {
    /// Runs one `EXTRACT` line and reports what it read, pass or fail.
    pub(super) async fn run_extract(
        &mut self,
        node: StepNode<'_>,
        plan: ExtractPlan,
        title: &str,
        budget: ActBudget,
        client: &mut ShimClient,
        state: &mut EntryState,
    ) -> (StepEnd, Option<ExtractReport>) {
        let (Some(model_client), Some(model)) = (self.run.model, self.options.model.clone()) else {
            let error = act_failure("extract-model", "EXTRACT needs the model option");
            return (StepEnd::Error(error), None);
        };
        let mut line = ActLine {
            node,
            title,
            deadline: Instant::now() + Duration::from_millis(budget.timeout_ms),
            budget,
            entry_start: state.steps.is_empty(),
            what: "EXTRACT",
        };
        let mut usage = PlanUsage::default();
        let end = self
            .extract_value(
                &mut line,
                &plan,
                &model,
                model_client,
                &mut usage,
                client,
                state,
            )
            .await;
        let (end, value) = match end {
            Ok(value) => (StepEnd::Passed, value),
            Err(end) => (end, None),
        };
        let reported = value.as_ref().map(|value| self.report_value(value));
        if matches!(end, StepEnd::Passed) {
            self.extracts.insert(plan.name.clone(), value);
        }
        let report = ExtractReport {
            model,
            value: reported,
            usage: usage_report(usage, false),
        };
        (end, Some(report))
    }

    /// The value, or the line's failure.
    async fn extract_value(
        &mut self,
        line: &mut ActLine<'_>,
        plan: &ExtractPlan,
        model: &str,
        model_client: &ModelClient,
        usage: &mut PlanUsage,
        client: &mut ShimClient,
        state: &mut EntryState,
    ) -> Result<Option<Value>, StepEnd> {
        let result = self
            .act_shim_call(
                line,
                StepCommand::AriaSnapshot {
                    locator: plan.scope.clone(),
                    settle:  true,
                },
                client,
                state,
            )
            .await
            .map_err(|failure| failure.end)?;
        let Ok(result) = serde_json::from_value::<AriaSnapshotResult>(result) else {
            return Err(StepEnd::Error(act_failure(
                "internal",
                "malformed ariaSnapshot result from the shim",
            )));
        };
        let snapshot = PageSnapshot::parse(&result.snapshot);
        let snapshot = if plan.scope.is_none() {
            snapshot.of_page()
        } else {
            snapshot
        };
        let (wire, wrapped) = extract::wire_schema(plan.schema.as_ref());
        let placeholders = plan.instruction.bindings().placeholders();
        let user = extract_message(plan.instruction.prompt(), &placeholders, snapshot.text());
        let reply = model_client
            .extract(model, &user, wire, line.deadline)
            .await;
        usage.model_calls = 1;
        let reply = match reply {
            Ok(reply) => reply,
            Err(error) => return Err(recode(self.model_failure(&error, line))),
        };
        usage.model = reply.usage;
        let text = reply.text.trim();
        let answer = if text.is_empty() {
            parse_json(&reply.object.to_string())
        } else {
            parse_json(text).or_else(|_| parse_json(&reply.object.to_string()))
        }
        .map_err(|error| {
            failure(
                "extract-schema",
                &format!("the answer is not JSON: {error}"),
            )
        })?;
        let mut value = extract::unwrap_answer(answer, plan.schema.as_ref(), wrapped);
        match &plan.schema {
            // A null answer is a missing value, whatever the schema (SPEC 7.6).
            Some(_) if matches!(value, Value::Null) => {}
            Some(schema) => {
                value = extract::read_numbers(value, schema);
                if let Some(problem) = extract::mismatch(&value, schema) {
                    return Err(failure(
                        "extract-schema",
                        &self
                            .vars
                            .mask(&format!("the answer does not match the schema: {problem}")),
                    ));
                }
            }
            None => {
                if !matches!(value, Value::String(_) | Value::Null) {
                    return Err(failure(
                        "extract-schema",
                        &format!(
                            "the answer is a {}, not a string",
                            value.value_type().name()
                        ),
                    ));
                }
            }
        }
        let value = match &plan.schema {
            Some(schema) => {
                self.resolve_links(line, value, schema, &snapshot, client, state)
                    .await?
            }
            None => value,
        };
        Ok((!extract::is_missing(&value, plan.schema.is_some())).then_some(value))
    }

    /// Replaces each link field's ref with the absolute `href` of its
    /// element (SPEC 7.6).
    async fn resolve_links(
        &mut self,
        line: &mut ActLine<'_>,
        value: Value,
        schema: &Json,
        snapshot: &PageSnapshot,
        client: &mut ShimClient,
        state: &mut EntryState,
    ) -> Result<Value, StepEnd> {
        let refs = extract::link_refs(&value, schema);
        if refs.is_empty() {
            return Ok(value);
        }
        let page = self
            .read_text(line, ReadSubject::url(), client, state)
            .await?
            .unwrap_or_default();
        let mut urls = HashMap::new();
        for element in refs {
            if urls.contains_key(&element) {
                continue;
            }
            if snapshot.target(&element).is_none() {
                return Err(failure(
                    "extract-ref",
                    &format!(
                        "the answer names element {element}, which is not in the page snapshot"
                    ),
                ));
            }
            let subject = ReadSubject::element_attr(&Locator::element_ref(&element), "href");
            let Some(href) = self.read_text(line, subject, client, state).await? else {
                return Err(failure(
                    "extract-ref",
                    &format!("element {element} is not a link with an href"),
                ));
            };
            let absolute = url::Url::parse(&page)
                .and_then(|base| base.join(&href))
                .map_or(href, |joined| joined.to_string());
            urls.insert(element, absolute);
        }
        Ok(extract::replace_links(value, schema, &mut |element| {
            urls.get(element).cloned()
        }))
    }

    /// One read of a page string; `None` when it is missing.
    async fn read_text(
        &mut self,
        line: &mut ActLine<'_>,
        subject: ReadSubject,
        client: &mut ShimClient,
        state: &mut EntryState,
    ) -> Result<Option<String>, StepEnd> {
        let result = self
            .act_shim_call(line, StepCommand::Read { subject }, client, state)
            .await
            .map_err(|failure| failure.end)?;
        Ok(match serde_json::from_value::<ReadResult>(result) {
            Ok(ReadResult::Value {
                value: Json::String(text),
            }) => Some(text),
            _ => None,
        })
    }

    /// A value as the report shows it: masked as a capture is (SPEC 14).
    fn report_value(&self, value: &Value) -> CaptureValue {
        let value_type = value.value_type().name();
        let text = value.text_form().unwrap_or_default();
        if self.vars.mask(&text) == text {
            CaptureValue::new(
                value_type,
                value.to_json().unwrap_or_else(|| "null".to_owned()),
            )
        } else {
            CaptureValue::masked(value_type, MASK)
        }
    }
}
