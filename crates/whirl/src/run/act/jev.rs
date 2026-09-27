//! The `--jev` planner (SPEC 7.4): TypeSafe's Jev chooses the action when
//! it is confident, and the language model plans otherwise.
//!
//! Jev answers closed questions in a few hundred milliseconds but cannot
//! write text. One request asks which kind of action the instruction wants.
//! A second asks which element, from the candidates that kind of action can
//! target, twice as Stagehand's Jev path does: `best` must choose, and
//! `strict` may say that none fits. Arguments come from the instruction
//! itself (see [`args`]). When Jev is unsure, when an argument is missing,
//! or when a request fails, the fallback planner plans the step, with Jev's
//! likely matches added to its prompt.

mod args;
mod client;
mod outline;

use std::collections::HashMap;
use std::fmt::Write as _;
use std::sync::Arc;
use std::time::Instant;

pub(crate) use client::{JevClient, JevSetupError};
use serde_json::{Map, Value as Json, json};

use self::client::{JevAnswer, JevResponse, choice, noul};
use self::outline::{Outline, View, shortlist};
use crate::run::act::decision::{ActInference, ActMethod};
use crate::run::act::planner::{
    ActPlanner, JevUsage, Plan, PlanFuture, PlanRequest, PlanStep, PlanUsage, PlannedBy,
};

/// The key of the none-of-these option.
const NONE: &str = "none_match";
const NONE_DESCRIPTION: &str = "None of these elements matches the instruction";
/// The confidence Jev needs to decide the intent or the element.
const ACCEPT: f64 = 0.7;
/// Strict's none-of-these vetoes a pick above this probability.
const NONE_VETO: f64 = 0.9;
/// Above this many candidates, Jev sees only the best word matches.
const PRUNE_ABOVE: usize = 40;
const PRUNE_KEEP: usize = 30;
/// Jev's likely matches reach the fallback when its top pick is at least
/// this likely.
const CREDIBLE: f64 = 0.5;
const HINT_ITEMS: usize = 5;

/// The kinds of action Jev can tell apart.
const INTENTS: &[(&str, &str)] = &[
    (
        "click",
        "Click, tap, open, follow, or toggle an element such as a button, link, tab, checkbox, or radio button",
    ),
    ("fill", "Type or enter text into a field"),
    ("select", "Choose an option from a dropdown list"),
    ("press", "Press a keyboard key, such as Enter or Escape"),
    (
        "hover",
        "Move the mouse over an element without clicking it",
    ),
    ("double_click", "Double-click an element"),
    (
        "other",
        "Something else, such as scrolling, dragging, or several actions at once",
    ),
];

/// Asks Jev first and the fallback planner when Jev is unsure.
#[derive(Debug)]
pub(crate) struct JevPlanner {
    jev:      JevClient,
    fallback: Arc<dyn ActPlanner>,
}

impl JevPlanner {
    pub(crate) fn new(jev: JevClient, fallback: Arc<dyn ActPlanner>) -> Self {
        Self { jev, fallback }
    }
}

/// What Jev decided for one planning step.
enum Outcome {
    Chosen(ActInference),
    /// The fallback plans, with Jev's likely matches when it has some.
    Unsure(Option<String>),
}

/// An element Jev chose, or what it found when it did not choose.
type Pick = Result<usize, Option<String>>;

impl ActPlanner for JevPlanner {
    fn plan<'a>(&'a self, request: PlanRequest<'a>) -> PlanFuture<'a> {
        Box::pin(async move {
            let mut jev = JevUsage::default();
            let outcome = self.choose(request, &mut jev).await;
            let spent = PlanUsage {
                jev,
                ..PlanUsage::default()
            };
            match outcome {
                Outcome::Chosen(inference) => Plan {
                    answer:     Ok(inference),
                    usage:      spent,
                    planned_by: PlannedBy::Jev,
                },
                Outcome::Unsure(hint) => {
                    let mut plan = self
                        .fallback
                        .plan(PlanRequest {
                            hint: hint.as_deref(),
                            ..request
                        })
                        .await;
                    plan.usage = plan.usage.saturating_add(spent);
                    plan
                }
            }
        })
    }

    fn name(&self) -> &'static str {
        PlannedBy::Jev.as_str()
    }
}

impl JevPlanner {
    async fn choose(&self, request: PlanRequest<'_>, usage: &mut JevUsage) -> Outcome {
        // Step two of a two-step action needs the model's plan of step one.
        if !matches!(request.step, PlanStep::First) {
            return Outcome::Unsure(None);
        }
        let instruction = request.instruction.prompt();
        let Some(intent) = self.intent(instruction, request.deadline, usage).await else {
            return Outcome::Unsure(None);
        };
        let outline = Outline::parse(request.snapshot.raw());
        let (method, view) = match intent {
            "click" => (ActMethod::Click, View::Pointer),
            "double_click" => (ActMethod::DoubleClick, View::Pointer),
            "hover" => (ActMethod::Hover, View::Pointer),
            "fill" => (ActMethod::Fill, View::Input),
            "select" => (ActMethod::SelectOptionFromDropdown, View::Select),
            "press" => (ActMethod::Press, View::Keyboard),
            _ => return Outcome::Unsure(None),
        };
        let key = match method {
            ActMethod::Press => match args::key(instruction) {
                Some(key) => Some(key),
                None => return Outcome::Unsure(None),
            },
            _ => None,
        };
        let focused = key.as_ref().and(outline.focused());
        let index = match focused {
            Some(index) => index,
            None => match self.pick(&outline, view, &request, usage).await {
                Ok(index) => index,
                Err(hint) => return Outcome::Unsure(hint),
            },
        };
        let node = outline.node(index);
        let description = describe_line(&outline.describe(index, &HashMap::new()));
        let argument = match method {
            ActMethod::Fill => args::fill_value(request.instruction, node.name.as_deref()),
            ActMethod::SelectOptionFromDropdown => {
                args::option(instruction, &outline.options(index), node.name.as_deref())
                    .map(str::to_owned)
            }
            ActMethod::Press => key,
            _ => None,
        };
        let needs_argument = !matches!(
            method,
            ActMethod::Click | ActMethod::DoubleClick | ActMethod::Hover
        );
        if needs_argument && argument.is_none() {
            // Jev found the element but cannot write its text.
            return Outcome::Unsure(Some(hint(&[(element(&outline, index), description)])));
        }
        Outcome::Chosen(ActInference::chosen(
            element(&outline, index),
            description,
            method,
            argument.into_iter().collect(),
        ))
    }

    /// Which kind of action the instruction asks for, when Jev is sure.
    async fn intent(
        &self,
        instruction: &str,
        deadline: Instant,
        usage: &mut JevUsage,
    ) -> Option<&'static str> {
        let criteria: Map<String, Json> = INTENTS
            .iter()
            .map(|(intent, meaning)| ((*intent).to_owned(), json!(meaning)))
            .collect();
        let mut questions = Map::new();
        questions.insert(
            "intent".to_owned(),
            choice(
                json!({
                    "question": "Which kind of browser action does the instruction ask for?",
                    "instruction": instruction,
                }),
                criteria,
            ),
        );
        let response = self.ask(instruction, questions, deadline, usage).await?;
        let JevAnswer::Choice {
            choice, confidence, ..
        } = response.answers.get("intent")?
        else {
            return None;
        };
        let intent = INTENTS
            .iter()
            .map(|(intent, _)| *intent)
            .find(|intent| intent == choice)?;
        (*confidence >= ACCEPT).then_some(intent)
    }

    /// The element Jev chooses among a view's candidates.
    async fn pick(
        &self,
        outline: &Outline,
        view: View,
        request: &PlanRequest<'_>,
        usage: &mut JevUsage,
    ) -> Pick {
        let instruction = request.instruction.prompt();
        let candidates = outline.view(view);
        if candidates.is_empty() {
            return Err(None);
        }
        let twins = outline.twins(&candidates);
        let descriptions: HashMap<usize, Json> = candidates
            .iter()
            .map(|&index| (index, outline.describe(index, &twins)))
            .collect();
        let asked = if candidates.len() > PRUNE_ABOVE {
            shortlist(instruction, &candidates, &descriptions, PRUNE_KEEP).ok_or(None)?
        } else {
            candidates
        };
        let criteria: Map<String, Json> = asked
            .iter()
            .map(|&index| (element(outline, index), descriptions[&index].clone()))
            .collect();
        let mut strict = criteria.clone();
        strict.insert(NONE.to_owned(), json!(NONE_DESCRIPTION));
        let single = match asked.as_slice() {
            [only] => Some(*only),
            _ => None,
        };
        let best = match single {
            Some(only) => noul(json!({
                "question": "Is this element a plausible target for the instruction?",
                "instruction": instruction,
                "element": descriptions[&only],
            })),
            None => choice(
                json!({
                    "question": instruction,
                    "note": "Pick the best available element even if the wording does not match exactly.",
                }),
                criteria,
            ),
        };
        let mut questions = Map::new();
        questions.insert("strict".to_owned(), choice(json!(instruction), strict));
        questions.insert("best".to_owned(), best);
        let response = self
            .ask(instruction, questions, request.deadline, usage)
            .await
            .ok_or(None)?;

        let none = match response.answers.get("strict") {
            Some(JevAnswer::Choice { probabilities, .. }) => {
                probabilities.get(NONE).copied().unwrap_or(0.0)
            }
            _ => return Err(None),
        };
        let by_ref: HashMap<String, usize> = asked
            .iter()
            .map(|&index| (element(outline, index), index))
            .collect();
        let (chosen, confidence, ranked) = match (single, response.answers.get("best")) {
            (Some(only), Some(JevAnswer::Noul { noul })) => (only, *noul, vec![(only, *noul)]),
            (
                None,
                Some(JevAnswer::Choice {
                    choice,
                    confidence,
                    probabilities,
                }),
            ) => {
                let Some(&chosen) = by_ref.get(choice) else {
                    return Err(None);
                };
                let mut ranked: Vec<(usize, f64)> = probabilities
                    .iter()
                    .filter_map(|(key, probability)| {
                        by_ref.get(key).map(|&index| (index, *probability))
                    })
                    .collect();
                ranked.sort_by(|left, right| right.1.total_cmp(&left.1));
                (chosen, *confidence, ranked)
            }
            _ => return Err(None),
        };
        if confidence >= ACCEPT && none <= NONE_VETO {
            return Ok(chosen);
        }
        if confidence < CREDIBLE {
            return Err(None);
        }
        let likely: Vec<(String, String)> = ranked
            .into_iter()
            .take(HINT_ITEMS)
            .map(|(index, _)| {
                (
                    element(outline, index),
                    describe_line(&descriptions[&index]),
                )
            })
            .collect();
        Err(Some(hint(&likely)))
    }

    /// One request; its usage counts even when it fails.
    async fn ask(
        &self,
        instruction: &str,
        questions: Map<String, Json>,
        deadline: Instant,
        usage: &mut JevUsage,
    ) -> Option<JevResponse> {
        usage.requests = usage.requests.saturating_add(1);
        let response = self
            .jev
            .ask(json!({ "instruction": instruction }), questions, deadline)
            .await
            .ok()?;
        usage.input_tokens = usage
            .input_tokens
            .saturating_add(response.usage.input_tokens);
        usage.output_tokens = usage
            .output_tokens
            .saturating_add(response.usage.output_tokens);
        Some(response)
    }
}

fn element(outline: &Outline, index: usize) -> String {
    outline
        .node(index)
        .element
        .clone()
        .expect("every candidate has a ref")
}

/// A description on one line, such as `button "Delete" · row: 1037 · Stark`.
fn describe_line(description: &Json) -> String {
    let Json::Object(fields) = description else {
        return description.to_string();
    };
    let mut parts = Vec::new();
    let role = fields
        .get("role")
        .and_then(Json::as_str)
        .unwrap_or_default();
    match fields.get("name").and_then(Json::as_str) {
        Some(name) => parts.push(format!("{role} {}", json!(name))),
        None => parts.push(role.to_owned()),
    }
    for (key, value) in fields {
        if key != "role" && key != "name" {
            let text = value
                .as_str()
                .map_or_else(|| value.to_string(), str::to_owned);
            parts.push(format!("{key}: {text}"));
        }
    }
    parts.join(" · ")
}

/// Jev's likely matches, as the fallback's prompt shows them.
fn hint(likely: &[(String, String)]) -> String {
    let mut text = String::from(
        "A classifier found these likely matches. Use one only if it fits the instruction and the tree:",
    );
    for (element, description) in likely {
        write!(text, "\n- {element}: {description}").expect("writing to a String cannot fail");
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_description_reads_on_one_line() {
        assert_eq!(
            describe_line(
                &json!({"role": "button", "name": "Delete", "row": "1037 · Stark", "position": "2 of 12"})
            ),
            r#"button "Delete" · row: 1037 · Stark · position: 2 of 12"#
        );
        assert_eq!(
            describe_line(&json!({"role": "textbox", "label": "Last name"})),
            "textbox · label: Last name"
        );
    }

    #[test]
    fn the_hint_lists_refs_and_descriptions() {
        assert_eq!(
            hint(&[("e5".to_owned(), "button \"Save\"".to_owned())]),
            "A classifier found these likely matches. Use one only if it fits the instruction and the tree:\n- e5: button \"Save\""
        );
    }
}
