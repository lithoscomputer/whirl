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
mod intent;
mod outline;

use std::collections::HashMap;
use std::fmt::Write as _;
use std::mem;
use std::sync::Arc;
use std::time::Instant;

pub(crate) use client::{JevClient, JevSetupError};
use serde_json::{Map, Value as Json, json};
use tokio::task::JoinSet;

use self::client::{JevAnswer, JevError, JevQuestion, JevResponse, choice, noul};
use self::intent::{FillValue, Intent};
use self::outline::{Outline, View, shortlist};
use crate::run::act::decision::{ActInference, ActMethod};
use crate::run::act::instruction::quoted_strings;
use crate::run::act::model::ModelClient;
use crate::run::act::planner::{
    ActPlanner, Plan, PlanFuture, PlanRequest, PlanStep, PlanUsage, PlannedBy,
};

/// The key of the none-of-these option.
const NONE: &str = "none_match";
const NONE_DESCRIPTION: &str = "None of these elements matches the instruction";
/// The confidence Jev needs to decide the intent or the element.
const ACCEPT: f64 = 0.7;
/// Strict's none-of-these vetoes a pick above this probability.
const NONE_VETO: f64 = 0.9;
/// Above this many candidates, Jev first sees only the best word matches.
const PRUNE_ABOVE: usize = 40;
const PRUNE_KEEP: usize = 30;
/// A pick that strict's none-of-these makes this uneasy is not trusted
/// from an exact-name match, is held while the next tier tries, and ends
/// the search when not accepted.
const NONE_UNEASY: f64 = 0.5;
/// A held pick is used only while none-of-these stays below this.
const HELD_NONE_MAX: f64 = 0.7;
/// The relaxed rule (Stagehand's eval findings): a split-but-right pick at
/// 0.5 or more, when strict is nearly certain something matches and the
/// leader is clear, at least 0.6 and 2.5 times the runner-up.
const SURE_ENOUGH: f64 = 0.5;
const NONE_CERTAIN: f64 = 0.1;
const CLEAR_LEADER: f64 = 0.6;
const LEADER_RATIO: f64 = 2.5;
/// The most options one request carries, and the most description
/// characters: about 4 characters a token against Jev's 32K-token limit
/// for the state and the longest question, with headroom.
const SHARD_SIZE: usize = 254;
const SHARD_CHARS: usize = 60_000;
/// The most parallel requests one pick makes.
const MAX_SHARDS: usize = 8;
/// Each part of a split list nominates this many candidates, each at
/// least this likely.
const NOMINEES_PER_SHARD: usize = 3;
const NOMINEE_MIN: f64 = 0.02;
/// Jev's likely matches reach the fallback when its top pick is at least
/// this likely.
const CREDIBLE: f64 = 0.5;
const HINT_ITEMS: usize = 5;

/// Roles whose click turns them on or off.
const CHECKABLE_ROLES: &[&str] = &[
    "checkbox",
    "switch",
    "radio",
    "menuitemcheckbox",
    "menuitemradio",
];

/// Asks Jev first and the fallback planner when Jev is unsure.
#[derive(Debug)]
pub(crate) struct JevPlanner {
    jev:      Arc<JevClient>,
    fallback: Arc<dyn ActPlanner>,
    /// Reads unquoted text to type from the instruction.
    model:    Arc<ModelClient>,
}

impl JevPlanner {
    pub(crate) fn new(
        jev: JevClient,
        fallback: Arc<dyn ActPlanner>,
        model: Arc<ModelClient>,
    ) -> Self {
        Self {
            jev: Arc::new(jev),
            fallback,
            model,
        }
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
            let mut spent = PlanUsage::default();
            let outcome = self.choose(request, &mut spent).await;
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
    async fn choose(&self, request: PlanRequest<'_>, usage: &mut PlanUsage) -> Outcome {
        // Step two of a two-step action needs the model's plan of step one.
        if !matches!(request.step, PlanStep::First) {
            return Outcome::Unsure(None);
        }
        let instruction = request.instruction.prompt();
        let fill_values: Vec<(String, bool)> = args::fill_values(request.instruction)
            .into_iter()
            .map(|value| {
                let placeholder = args::is_placeholder(request.instruction, &value);
                (value, placeholder)
            })
            .collect();
        // A lone placeholder is the text to type; nothing to ask.
        let lone_placeholder = match fill_values.as_slice() {
            [(value, true)] => Some(value.clone()),
            _ => None,
        };
        let offered: &[(String, bool)] = if lone_placeholder.is_some() {
            &[]
        } else {
            &fill_values
        };
        let Some(intent) = self
            .intent(instruction, offered, request.deadline, usage)
            .await
        else {
            return Outcome::Unsure(None);
        };
        // Whirl clicks with the left button only, and choosing a suggestion
        // after typing is a second step.
        if intent.other_button || (intent.family == "fill" && intent.pick_suggestion) {
            return Outcome::Unsure(None);
        }
        let outline = Outline::parse(request.snapshot.raw());
        // A merged click-or-select vote on a page with no native select is
        // a click on a custom control.
        let family = if intent.family == "select"
            && intent.click_fits
            && outline.view(View::Select).is_empty()
        {
            "click"
        } else {
            intent.family
        };
        let (method, tiers): (ActMethod, &[View]) = match family {
            "click" => (ActMethod::Click, &[View::Pointer, View::Broad]),
            "double_click" => (ActMethod::DoubleClick, &[View::Pointer, View::Broad]),
            "hover" => (ActMethod::Hover, &[View::Pointer, View::Broad]),
            "fill" => (ActMethod::Fill, &[View::Input, View::Broad]),
            "select" => (ActMethod::SelectOptionFromDropdown, &[View::Select]),
            "press" => (ActMethod::Press, &[View::Keyboard, View::Broad]),
            _ => return Outcome::Unsure(None),
        };
        // The text to type, when Jev already named it; the other quoted
        // strings may name the target.
        let typed = match (&lone_placeholder, intent.fill_value) {
            (Some(value), _) => Some(value.clone()),
            (None, FillValue::Chosen(index)) => {
                fill_values.get(index).map(|(value, _)| value.clone())
            }
            _ => None,
        };
        let quoted: Vec<&str> = quoted_strings(instruction)
            .into_iter()
            .filter(|quoted| typed.as_deref() != Some(*quoted))
            .collect();
        let key = match method {
            ActMethod::Press => match intent
                .key
                .map(str::to_owned)
                .or_else(|| args::key(instruction))
            {
                Some(key) => Some(key),
                None => return Outcome::Unsure(None),
            },
            _ => None,
        };
        let focused = key.as_ref().and(outline.focused());
        let index = match focused {
            Some(index) => index,
            None => match self.pick(&outline, tiers, &request, &quoted, usage).await {
                Ok(index) => index,
                Err(hint) => return Outcome::Unsure(hint),
            },
        };
        let node = outline.node(index);
        let description = describe_line(&outline.describe(index, &HashMap::new()));
        // A click toggles; when the control is already in the state the
        // instruction asks for, a click would undo it.
        if method == ActMethod::Click
            && CHECKABLE_ROLES.contains(&node.role.as_str())
            && intent.toggle == Some(node.checked)
        {
            return Outcome::Unsure(Some(hint(&[(element(&outline, index), description)])));
        }
        let argument = match method {
            ActMethod::Fill => match (typed, intent.fill_value) {
                (Some(value), _) => Some(value),
                (None, FillValue::NotAsked | FillValue::NoneOfThese) => {
                    self.text_argument(&request, usage).await
                }
                (None, FillValue::Chosen(_) | FillValue::Unsure) => None,
            },
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

    /// What kind of action the instruction asks for, and its details, when
    /// Jev is sure of the kind.
    async fn intent(
        &self,
        instruction: &str,
        fill_values: &[(String, bool)],
        deadline: Instant,
        usage: &mut PlanUsage,
    ) -> Option<Intent> {
        let questions = intent::questions(instruction, fill_values);
        let response = self.ask(instruction, questions, deadline, usage).await?;
        intent::read(&response.answers, ACCEPT)
    }

    /// The text to type when the instruction does not quote it: a small
    /// model call that sees only the instruction, and whose answer must be
    /// the instruction's own words (Stagehand's argument call).
    async fn text_argument(
        &self,
        request: &PlanRequest<'_>,
        usage: &mut PlanUsage,
    ) -> Option<String> {
        let instruction = request.instruction;
        let reply = self
            .model
            .text_argument(
                request.model,
                instruction.prompt(),
                &instruction.bindings().placeholders(),
                request.deadline,
            )
            .await;
        usage.model_calls = usage.model_calls.saturating_add(1);
        let reply = reply.ok()?;
        usage.model = usage.model.saturating_add(reply.usage);
        instruction.span(&reply.text?)
    }

    /// The element Jev chooses, trying each tier of candidates in turn: the
    /// view for the action, then every named element. Within a tier, a long
    /// list is first cut to its best word matches, and the full list is
    /// asked only when Jev rejects the cut one. This is Stagehand's
    /// `pickTarget`. `quoted` are the instruction's quoted strings that are
    /// not the text to type.
    async fn pick(
        &self,
        outline: &Outline,
        tiers: &[View],
        request: &PlanRequest<'_>,
        quoted: &[&str],
        usage: &mut PlanUsage,
    ) -> Pick {
        let instruction = request.instruction.prompt();
        let mut seen: Vec<Vec<usize>> = Vec::new();
        let mut ranked: Vec<(usize, f64)> = Vec::new();
        let mut described: HashMap<usize, Json> = HashMap::new();
        // An accepted pick that strict is uneasy about: kept while the next
        // tier tries, since the real target may be outside this view.
        let mut held: Option<Judged> = None;
        let found =
            |held: Option<Judged>, ranked: &[(usize, f64)], described: &HashMap<usize, Json>| {
                match held
                    .filter(|held| held.none <= HELD_NONE_MAX)
                    .and_then(|held| held.chosen)
                {
                    Some(chosen) => Ok(chosen),
                    None => Err(likely(outline, ranked, described)),
                }
            };
        for &view in tiers {
            let candidates = outline.view(view);
            if candidates.is_empty() || seen.contains(&candidates) {
                continue;
            }
            seen.push(candidates.clone());
            let twins = outline.twins(&candidates);
            let descriptions: HashMap<usize, Json> = candidates
                .iter()
                .map(|&index| (index, outline.describe(index, &twins)))
                .collect();
            described.extend(
                descriptions
                    .iter()
                    .map(|(index, json)| (*index, json.clone())),
            );

            // One quoted name that exactly one candidate has: a small
            // request confirms it, since the quote may only be an anchor
            // ("the link below 'Pricing'").
            if let [name] = quoted
                && let [only] = exact_name_matches(outline, &candidates, name).as_slice()
            {
                let judged = self
                    .judge(outline, &[*only], &descriptions, request, false, usage)
                    .await;
                if judged.accepted && judged.none <= NONE_UNEASY {
                    return Ok(*only);
                }
            }

            let mut attempts = Vec::new();
            if candidates.len() > PRUNE_ABOVE
                && let Some(pruned) = shortlist(instruction, &candidates, &descriptions, PRUNE_KEEP)
                && pruned.len() < candidates.len()
            {
                attempts.push(pruned);
            }
            attempts.push(candidates);
            for attempt in attempts {
                // The broad tier is a last look, not worth several requests.
                let allow_shards = view != View::Broad;
                let judged = self
                    .judge(
                        outline,
                        &attempt,
                        &descriptions,
                        request,
                        allow_shards,
                        usage,
                    )
                    .await;
                if !judged.ranked.is_empty() {
                    ranked.clone_from(&judged.ranked);
                }
                match judged.chosen {
                    Some(chosen) if judged.accepted && judged.none <= NONE_UNEASY => {
                        return Ok(chosen);
                    }
                    Some(_) if judged.accepted => {
                        let margin = |judged: &Judged| judged.confidence - judged.none;
                        if held
                            .as_ref()
                            .is_none_or(|held| margin(&judged) > margin(held))
                        {
                            held = Some(judged);
                        }
                        break;
                    }
                    // Jev saw plausible targets and could not choose between
                    // them; a wider list will not make that easier.
                    Some(_) if judged.none < NONE_UNEASY => {
                        return found(held, &ranked, &described);
                    }
                    _ => {}
                }
            }
        }
        found(held, &ranked, &described)
    }

    /// Jev's verdict on one list of candidates: `best` must choose, and
    /// `strict` may say none fits (Stagehand's `pickCandidate`). A list too
    /// large for one request is split into parts that each nominate their
    /// likeliest candidates, in parallel, and the nominees are then judged
    /// together.
    async fn judge(
        &self,
        outline: &Outline,
        candidates: &[usize],
        descriptions: &HashMap<usize, Json>,
        request: &PlanRequest<'_>,
        allow_shards: bool,
        usage: &mut PlanUsage,
    ) -> Judged {
        let instruction = request.instruction.prompt();
        let shards = shard(candidates, |index| descriptions[&index].to_string().len());
        if shards.len() > if allow_shards { MAX_SHARDS } else { 1 } {
            return Judged::rejected();
        }
        let finalists = if shards.len() > 1 {
            self.nominate(outline, &shards, descriptions, request, usage)
                .await
        } else {
            candidates.to_vec()
        };
        if finalists.is_empty() {
            return Judged::rejected();
        }

        let criteria: Map<String, Json> = finalists
            .iter()
            .map(|&index| (element(outline, index), descriptions[&index].clone()))
            .collect();
        let mut strict = criteria.clone();
        strict.insert(NONE.to_owned(), json!(NONE_DESCRIPTION));
        let single = match finalists.as_slice() {
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
        let questions = vec![
            ("strict", choice(json!(instruction), strict)),
            ("best", best),
        ];
        let Some(response) = self
            .ask(instruction, questions, request.deadline, usage)
            .await
        else {
            return Judged::rejected();
        };

        let none = match response.answers.get("strict") {
            Some(JevAnswer::Choice { probabilities, .. }) => {
                probabilities.get(NONE).copied().unwrap_or(0.0)
            }
            _ => return Judged::rejected(),
        };
        let by_ref: HashMap<String, usize> = finalists
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
                    return Judged::rejected();
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
            _ => return Judged::rejected(),
        };
        let mut chosen = chosen;
        let mut accepted = accepts(confidence, none, &ranked);
        // A vote split between copies of one control in one item: either
        // is the answer.
        if !accepted
            && none <= NONE_UNEASY
            && let [(first, first_p), (second, second_p), ..] = ranked.as_slice()
            && first_p + second_p >= ACCEPT
            && outline.copies_in_one_item(*first, *second)
            && without_position(&descriptions[first]) == without_position(&descriptions[second])
        {
            accepted = true;
            chosen = *first;
        }
        Judged {
            chosen: Some(chosen),
            accepted,
            confidence,
            none,
            ranked,
        }
    }

    /// Each part of a split list nominates its likeliest candidates, in
    /// parallel requests.
    async fn nominate(
        &self,
        outline: &Outline,
        shards: &[Vec<usize>],
        descriptions: &HashMap<usize, Json>,
        request: &PlanRequest<'_>,
        usage: &mut PlanUsage,
    ) -> Vec<usize> {
        let instruction = request.instruction.prompt();
        let mut tasks = JoinSet::new();
        for (position, shard) in shards.iter().enumerate() {
            let mut options: Map<String, Json> = shard
                .iter()
                .map(|&index| (element(outline, index), descriptions[&index].clone()))
                .collect();
            options.insert(NONE.to_owned(), json!(NONE_DESCRIPTION));
            let questions = vec![("shard", choice(json!(instruction), options))];
            let state = json!({ "instruction": instruction });
            let jev = Arc::clone(&self.jev);
            let deadline = request.deadline;
            tasks.spawn(async move { (position, jev.ask(state, questions, deadline).await) });
        }
        let mut answers: Vec<Option<JevResponse>> = vec![None; shards.len()];
        while let Some(joined) = tasks.join_next().await {
            let Ok((position, result)) = joined else {
                continue;
            };
            answers[position] = record(usage, result);
        }
        let mut finalists = Vec::new();
        for (shard, answer) in shards.iter().zip(answers) {
            let Some(JevAnswer::Choice { probabilities, .. }) =
                answer.and_then(|response| response.answers.get("shard").cloned())
            else {
                continue;
            };
            let by_ref: HashMap<String, usize> = shard
                .iter()
                .map(|&index| (element(outline, index), index))
                .collect();
            let mut nominees: Vec<(usize, f64)> = probabilities
                .iter()
                .filter(|(_, probability)| **probability >= NOMINEE_MIN)
                .filter_map(|(key, probability)| {
                    by_ref.get(key).map(|&index| (index, *probability))
                })
                .collect();
            nominees.sort_by(|left, right| right.1.total_cmp(&left.1));
            finalists.extend(
                nominees
                    .into_iter()
                    .take(NOMINEES_PER_SHARD)
                    .map(|(index, _)| index),
            );
        }
        finalists.truncate(SHARD_SIZE);
        finalists
    }

    /// One request; its usage counts even when it fails.
    async fn ask(
        &self,
        instruction: &str,
        questions: Vec<(&'static str, JevQuestion)>,
        deadline: Instant,
        usage: &mut PlanUsage,
    ) -> Option<JevResponse> {
        let result = self
            .jev
            .ask(json!({ "instruction": instruction }), questions, deadline)
            .await;
        record(usage, result)
    }
}

/// Counts one Jev request and what its answer cost.
fn record(usage: &mut PlanUsage, result: Result<JevResponse, JevError>) -> Option<JevResponse> {
    let usage = &mut usage.jev;
    usage.requests = usage.requests.saturating_add(1);
    let response = result.ok()?;
    let spend = response.spend;
    usage.input_tokens = usage.input_tokens.saturating_add(spend.input_tokens);
    usage.output_tokens = usage.output_tokens.saturating_add(spend.output_tokens);
    match spend.cost_usd_micros {
        Some(cost) => usage.cost_usd_micros = usage.cost_usd_micros.saturating_add(cost),
        None => usage.unpriced = true,
    }
    Some(response)
}

/// Whether Jev's pick is sure enough to act on: confident with strict not
/// vetoing it, or a clear leader when strict is nearly certain something
/// matches.
fn accepts(confidence: f64, none: f64, ranked: &[(usize, f64)]) -> bool {
    let leader = ranked.first().map_or(0.0, |(_, probability)| *probability);
    let runner_up = ranked.get(1).map_or(0.0, |(_, probability)| *probability);
    let clear_leader = leader >= CLEAR_LEADER && leader >= LEADER_RATIO * runner_up;
    (confidence >= ACCEPT && none <= NONE_VETO)
        || (confidence >= SURE_ENOUGH && none <= NONE_CERTAIN && clear_leader)
}

/// A description without its place among look-alikes.
fn without_position(description: &Json) -> Json {
    let mut description = description.clone();
    if let Json::Object(fields) = &mut description {
        fields.remove("position");
    }
    description
}

/// Jev's verdict on one list of candidates.
#[derive(Debug)]
struct Judged {
    chosen:     Option<usize>,
    accepted:   bool,
    /// `best`'s confidence in the chosen candidate.
    confidence: f64,
    /// Strict's probability that none of the candidates fits.
    none:       f64,
    /// Candidates by Jev's probability, likeliest first.
    ranked:     Vec<(usize, f64)>,
}

impl Judged {
    fn rejected() -> Self {
        Self {
            chosen:     None,
            accepted:   false,
            confidence: 0.0,
            none:       1.0,
            ranked:     Vec::new(),
        }
    }
}

/// Splits candidates into parts that each fit one request: at most
/// [`SHARD_SIZE`] options and [`SHARD_CHARS`] characters of descriptions.
fn shard(candidates: &[usize], size: impl Fn(usize) -> usize) -> Vec<Vec<usize>> {
    let mut shards: Vec<Vec<usize>> = Vec::new();
    let mut current: Vec<usize> = Vec::new();
    let mut chars = 0;
    for &index in candidates {
        let cost = size(index) + 16;
        if !current.is_empty() && (current.len() >= SHARD_SIZE || chars + cost > SHARD_CHARS) {
            shards.push(mem::take(&mut current));
            chars = 0;
        }
        current.push(index);
        chars += cost;
    }
    if !current.is_empty() {
        shards.push(current);
    }
    shards
}

/// The candidates whose name is exactly `name`, ignoring case.
fn exact_name_matches(outline: &Outline, candidates: &[usize], name: &str) -> Vec<usize> {
    let name = name.trim().to_lowercase();
    candidates
        .iter()
        .copied()
        .filter(|&index| {
            outline
                .node(index)
                .name
                .as_deref()
                .is_some_and(|candidate| candidate.trim().to_lowercase() == name)
        })
        .collect()
}

/// Jev's likely matches for the fallback, when its top pick is credible.
fn likely(
    outline: &Outline,
    ranked: &[(usize, f64)],
    descriptions: &HashMap<usize, Json>,
) -> Option<String> {
    if ranked
        .first()
        .is_none_or(|(_, probability)| *probability < CREDIBLE)
    {
        return None;
    }
    let likely: Vec<(String, String)> = ranked
        .iter()
        .take(HINT_ITEMS)
        .map(|&(index, _)| {
            let description = descriptions
                .get(&index)
                .map_or_else(String::new, describe_line);
            (element(outline, index), description)
        })
        .collect();
    Some(hint(&likely))
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
    fn a_long_list_splits_by_count_and_by_characters() {
        let many: Vec<usize> = (0..600).collect();
        let by_count = shard(&many, |_| 10);
        assert_eq!(by_count.iter().map(Vec::len).collect::<Vec<_>>(), [
            SHARD_SIZE,
            SHARD_SIZE,
            600 - 2 * SHARD_SIZE
        ]);
        let wordy = shard(&many[..10], |_| SHARD_CHARS / 3);
        assert_eq!(wordy.iter().map(Vec::len).collect::<Vec<_>>(), [
            2, 2, 2, 2, 2
        ]);
    }

    #[test]
    fn an_exact_name_ignores_case_and_outer_spaces() {
        let outline = Outline::parse(
            "- generic [ref=e1]:\n  - link \"Pricing\" [ref=e2]\n  - link \"Pricing plans\" [ref=e3]\n",
        );
        let candidates = outline.view(View::Pointer);
        assert_eq!(exact_name_matches(&outline, &candidates, " pricing "), [
            candidates[0]
        ]);
    }

    #[test]
    fn likely_matches_need_a_credible_leader() {
        let outline = Outline::parse("- generic [ref=e1]:\n  - button \"Save\" [ref=e5]\n");
        let save = outline.view(View::Pointer)[0];
        let descriptions = HashMap::from([(save, json!({"role": "button", "name": "Save"}))]);
        assert_eq!(likely(&outline, &[(save, 0.3)], &descriptions), None);
        assert!(
            likely(&outline, &[(save, 0.6)], &descriptions)
                .is_some_and(|hint| hint.ends_with("- e5: button \"Save\""))
        );
    }

    #[test]
    fn a_clear_leader_is_accepted_when_strict_is_nearly_certain() {
        // Confident and not vetoed.
        assert!(accepts(0.75, 0.8, &[(1, 0.75), (2, 0.25)]));
        assert!(!accepts(0.75, 0.95, &[(1, 0.75), (2, 0.25)]));
        // Split but right: a clear leader, strict certain something fits.
        assert!(accepts(0.55, 0.05, &[(1, 0.62), (2, 0.2)]));
        // Not clear: 0.56 against 0.38.
        assert!(!accepts(0.55, 0.05, &[(1, 0.56), (2, 0.38)]));
        // Clear, but strict is not certain.
        assert!(!accepts(0.55, 0.3, &[(1, 0.62), (2, 0.2)]));
    }

    #[test]
    fn the_hint_lists_refs_and_descriptions() {
        assert_eq!(
            hint(&[("e5".to_owned(), "button \"Save\"".to_owned())]),
            "A classifier found these likely matches. Use one only if it fits the instruction and the tree:\n- e5: button \"Save\""
        );
    }
}
