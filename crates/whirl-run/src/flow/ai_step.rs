//! `ai:` targets (SPEC 6.3): a language model finds the one element that a
//! description names, and the line runs on it through a snapshot ref. The
//! AI cache (SPEC 12.1) replays what a target resolved to.
//!
//! An action or a state check resolves its targets first and then runs as
//! usual. A check or capture with a subject resolves inside its read loop
//! (see `check_step`), so it resolves again when its element goes away.

use std::mem;
use std::time::{Duration, Instant};

use tokio::time::sleep;
use whirl_ai::{Fingerprint, Instruction, PageSnapshot, PlanUsage, Target, target_message};
use whirl_lang::ast::{self, Locator, LocatorSegment, SegmentKind};
use whirl_lang::{is_role, parse_locator, render_snapshot_target};
use whirl_report::model::{
    ActReport, AiReport, AiTargetReport, ExtractReport, JudgeReport, StepError, StepWarning,
};
use whirl_shim::{AriaSnapshotResult, GeneratedLocator, ShimClient, StepCommand};
use whirl_types::PredicateKind;

use super::act_step::{ActBudget, ActLine, act_failure, usage_report};
use super::{EntryState, FlowExec, PreparedStep, StepEnd, StepNode, entry_timeout_error};
use crate::cache::{CacheEntry, CacheKey, EntryKind};

/// Whirl asks the model again for a target it did not find at most this
/// often (SPEC 6.3).
pub(super) const ASK_INTERVAL: Duration = Duration::from_secs(2);

/// One `ai:` target of a line while the line runs.
#[derive(Clone, Debug)]
pub(super) struct AiTarget {
    /// The authored locator, ending in its `ai:` segment.
    authored:    Locator,
    /// True in a `hidden` or `not exists` check: no match passes.
    absence_ok:  bool,
    /// The target's cache key (SPEC 12.1).
    key:         CacheKey,
    /// When Whirl last asked the model.
    last_ask:    Option<Instant>,
    /// True when the last answer found no element.
    absent:      bool,
    /// True once Whirl tried the target's cache entry.
    cache_tried: bool,
    /// The cache entry that fit the page.
    hit:         Option<CacheEntry>,
    /// The locator and fingerprint to cache for the element the model
    /// found.
    found:       Option<(String, Fingerprint)>,
    /// Why the element the model found cannot be cached: a warning code
    /// and its reason.
    uncacheable: Option<(&'static str, String)>,
    report:      AiTargetReport,
}

impl AiTarget {
    /// Whether the model may be asked again now.
    pub(super) fn may_ask(&self) -> bool {
        self.last_ask
            .is_none_or(|asked| asked.elapsed() >= ASK_INTERVAL)
    }

    /// The target's report, as it stands.
    pub(super) fn report_owned(self) -> AiTargetReport {
        self.report
    }

    /// The locator the line uses in place of the target: the element's
    /// snapshot ref.
    fn element_locator(&self, element: &str) -> Locator {
        Locator {
            segments: vec![LocatorSegment {
                kind: SegmentKind::Ref(element.to_owned()),
                span: self.authored.span,
            }],
            span:     self.authored.span,
        }
    }

    /// The segments before `ai:`, which limit the snapshot. A final frame
    /// becomes its iframe element, whose snapshot shows the frame's page.
    fn scope(&self) -> Option<Locator> {
        let mut segments: Vec<LocatorSegment> = self
            .authored
            .segments
            .iter()
            .take(self.authored.segments.len().saturating_sub(1))
            .cloned()
            .collect();
        if segments.is_empty() {
            return None;
        }
        if let Some(frame) = segments
            .iter_mut()
            .rev()
            .find(|segment| !matches!(segment.kind, SegmentKind::Nth(_)))
            && let SegmentKind::Frame(selector) = &frame.kind
        {
            frame.kind = SegmentKind::Css(selector.clone());
        }
        Some(Locator {
            segments,
            span: self.authored.span,
        })
    }

    /// The warnings of a line that passed (SPEC 12.1).
    fn warnings(&self) -> Vec<StepWarning> {
        let warning = |code: &str, message: String| StepWarning {
            code: code.to_owned(),
            message,
        };
        let target = &self.report.target;
        if self.absent {
            return vec![warning(
                "uncached",
                format!("{target} matched no element, so the model is asked on every run"),
            )];
        }
        if self.hit.is_some() {
            return Vec::new();
        }
        if let Some((code, reason)) = &self.uncacheable {
            return vec![warning(
                code,
                format!("{target} is not cached, so the model is asked on every run: {reason}"),
            )];
        }
        let Some((locator, _)) = &self.found else {
            return Vec::new();
        };
        match &self.report.cached {
            Some(cached) => vec![warning(
                "healed",
                format!("{target}: the cached {cached} no longer fits; the model found {locator}"),
            )],
            None => vec![warning(
                "cache-miss",
                format!("{target} has no cache entry; the model found {locator}"),
            )],
        }
    }
}

/// What one ask found.
pub(super) enum Found {
    /// One element; the locator to use in the target's place.
    One(Locator),
    /// No element.
    Nothing,
}

/// The targets of one line, what their model calls used, and the
/// warnings of a line that passed.
#[derive(Debug, Default)]
pub(super) struct AiSpend {
    pub(super) targets:  Vec<AiTargetReport>,
    pub(super) usage:    PlanUsage,
    pub(super) warnings: Vec<StepWarning>,
}

impl AiSpend {
    /// The step's `ai` report; `None` for a line without targets.
    pub(super) fn report(&mut self, model: Option<String>) -> Option<AiReport> {
        if self.targets.is_empty() {
            return None;
        }
        Some(AiReport {
            model:   model.unwrap_or_default(),
            targets: mem::take(&mut self.targets),
            usage:   usage_report(self.usage, false),
        })
    }
}

/// True when a check line reads its element only to test that it is gone:
/// `not exists` (SPEC 6.3).
pub(super) fn checks_absence(line: &ast::CheckLine) -> bool {
    line.negated && line.predicate.kind() == PredicateKind::Exists
}

fn strictness(target: &AiTarget, candidates: Vec<String>) -> StepError {
    StepError {
        code: "strictness".to_owned(),
        message: format!(
            "strictness: {} matches {} elements",
            target.report.target,
            candidates.len()
        ),
        candidates: Some(candidates),
        ..StepError::default()
    }
}

/// The failure of a miss in `--cache=only` (SPEC 12.1).
pub(super) fn cache_miss(what: &str) -> StepEnd {
    StepEnd::Failed(StepError {
        code: "cache-miss".to_owned(),
        message: format!(
            "cache-miss: {what} has no cache entry that fits the page, and --cache=only asks no model"
        ),
        ..StepError::default()
    })
}

fn variable_failure(message: String) -> StepEnd {
    StepEnd::Failed(StepError {
        code: "variable-resolution".to_owned(),
        message,
        ..StepError::default()
    })
}

impl FlowExec<'_> {
    /// A target of the line `node` (SPEC 6.3), with its cache key.
    pub(super) fn ai_target(
        &self,
        node: StepNode<'_>,
        authored: &Locator,
        absence_ok: bool,
    ) -> AiTarget {
        let text = render_snapshot_target(authored);
        let key = self.cache.key(
            EntryKind::AiTarget,
            node.line(),
            node.raw_text(),
            Some(text.clone()),
        );
        AiTarget {
            authored: authored.clone(),
            absence_ok,
            key,
            last_ask: None,
            absent: false,
            cache_tried: false,
            hit: None,
            found: None,
            uncacheable: None,
            report: AiTargetReport {
                target: self.vars.mask(&text),
                cache: "miss".to_owned(),
                ..AiTargetReport::default()
            },
        }
    }

    /// Keeps a passing line's target in the cache and returns its warnings
    /// (SPEC 12.1).
    pub(super) fn finish_target(
        &mut self,
        node: StepNode<'_>,
        mut target: AiTarget,
        spend: &mut AiSpend,
    ) {
        spend.warnings.extend(target.warnings());
        if target.absent {
            "uncached".clone_into(&mut target.report.cache);
        } else if let Some(entry) = target.hit.clone() {
            self.cache.keep(node.line(), entry);
        } else if let (Some((locator, fingerprint)), Some(model)) =
            (target.found.clone(), self.options.model.clone())
        {
            let CacheKey {
                line,
                occurrence,
                target: target_text,
                ..
            } = target.key.clone();
            self.cache.keep(node.line(), CacheEntry::AiTarget {
                line,
                occurrence,
                target: target_text.unwrap_or_default(),
                model,
                locator,
                fingerprint,
            });
        } else if target.uncacheable.is_some() {
            "uncached".clone_into(&mut target.report.cache);
        }
        spend.targets.push(target.report);
    }

    /// Checks that a cached locator finds exactly one element with the
    /// cached fingerprint, waiting at most half of the line's remaining
    /// time (SPEC 12.1). `None` is a miss.
    pub(super) async fn check_cached(
        &mut self,
        line: &ActLine<'_>,
        locator: &Locator,
        fingerprint: &Fingerprint,
        client: &mut ShimClient,
        state: &mut EntryState,
    ) -> Option<()> {
        let locator = self.locator(locator, None).ok()?;
        let half = Duration::from_millis(line.remaining_ms() / 2);
        let mut probe = ActLine {
            deadline: Instant::now() + half,
            entry_start: line.entry_start,
            ..*line
        };
        let result = self
            .act_shim_call(
                &mut probe,
                StepCommand::AriaSnapshot {
                    locator: Some(locator),
                    settle:  false,
                },
                client,
                state,
            )
            .await
            .ok()?;
        let snapshot = serde_json::from_value::<AriaSnapshotResult>(result).ok()?;
        (Fingerprint::of_snapshot(&snapshot.snapshot).as_ref() == Some(fingerprint)).then_some(())
    }

    /// Tries the target's cache entry once (SPEC 12.1). A hit returns the
    /// cached locator; a miss records the entry as the one a heal
    /// replaces.
    async fn try_cache(
        &mut self,
        line: &ActLine<'_>,
        target: &mut AiTarget,
        client: &mut ShimClient,
        state: &mut EntryState,
    ) -> Option<Locator> {
        if target.cache_tried {
            return None;
        }
        target.cache_tried = true;
        let entry = self.cache.get(&target.key).cloned()?;
        let CacheEntry::AiTarget {
            locator: text,
            fingerprint,
            ..
        } = &entry
        else {
            return None;
        };
        let locator = parse_locator(text).ok();
        let hit = match &locator {
            Some(locator) => self
                .check_cached(line, locator, fingerprint, client, state)
                .await
                .is_some(),
            None => false,
        };
        if hit {
            "hit".clone_into(&mut target.report.cache);
            target.report.locator = Some(self.vars.mask(text));
            target.hit = Some(entry.clone());
            return locator;
        }
        "healed".clone_into(&mut target.report.cache);
        target.report.cached = Some(self.vars.mask(text));
        None
    }

    /// Finds the target's element from its cache entry, or else by asking
    /// the model once (SPEC 6.3, 12.1). In `--cache=only`, a miss fails
    /// unless no element passes.
    pub(super) async fn find_target(
        &mut self,
        line: &mut ActLine<'_>,
        target: &mut AiTarget,
        usage: &mut PlanUsage,
        client: &mut ShimClient,
        state: &mut EntryState,
    ) -> Result<Found, StepEnd> {
        if let Some(locator) = self.try_cache(line, target, client, state).await {
            return Ok(Found::One(locator));
        }
        if !self.cache.mode().allows_model() && !target.absence_ok {
            return Err(cache_miss(&target.report.target));
        }
        self.ask_target(line, target, usage, client, state).await
    }

    /// Asks the model once for the target's element (SPEC 6.3).
    async fn ask_target(
        &mut self,
        line: &mut ActLine<'_>,
        target: &mut AiTarget,
        usage: &mut PlanUsage,
        client: &mut ShimClient,
        state: &mut EntryState,
    ) -> Result<Found, StepEnd> {
        let (Some(model_client), Some(model)) = (self.run.model, self.options.model.clone()) else {
            return Err(StepEnd::Error(act_failure(
                "act-model",
                "ai: needs the model option",
            )));
        };
        let description = target
            .authored
            .ai_description()
            .expect("an AI target ends in an ai: segment")
            .clone();
        let scope = match target.scope() {
            Some(scope) => Some(
                self.locator(&scope, None)
                    .map_err(|error| variable_failure(self.vars.mask(&error.to_string())))?,
            ),
            None => None,
        };
        let whole_page = scope.is_none();
        let result = self
            .act_shim_call(
                line,
                StepCommand::AriaSnapshot {
                    locator: scope,
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
        let snapshot = if whole_page {
            snapshot.of_page()
        } else {
            snapshot
        };
        let instruction = Instruction::try_new(&description, &mut self.vars)
            .map_err(|error| variable_failure(self.vars.mask(&error.to_string())))?;
        let placeholders = instruction.bindings().placeholders();
        let user = target_message(instruction.prompt(), &placeholders, snapshot.text());
        let reply = model_client
            .find_elements(&model, &user, line.deadline)
            .await;
        target.last_ask = Some(Instant::now());
        usage.model_calls = usage.model_calls.saturating_add(1);
        let reply = match reply {
            Ok(reply) => reply,
            Err(error) => return Err(self.model_failure(&error, line)),
        };
        usage.model = usage.model.saturating_add(reply.usage);
        let answer = reply.answer.map_err(|error| {
            StepEnd::Failed(act_failure(
                "act-invalid-decision",
                &format!("the model's answer does not match the ai: schema: {error}"),
            ))
        })?;
        let mut found: Vec<(Target, String)> = Vec::new();
        let mut unknown = None;
        for element in answer.elements {
            match snapshot.target(&element.element_id) {
                Some(candidate) => {
                    if !found.iter().any(|(known, _)| known == &candidate) {
                        found.push((candidate, element.description));
                    }
                }
                None => unknown = Some(element.element_id),
            }
        }
        match found.len() {
            0 => {
                if let Some(element_id) = unknown {
                    return Err(StepEnd::Failed(act_failure(
                        "act-invalid-decision",
                        &format!(
                            "the answer names element {element_id}, which is not in the page snapshot"
                        ),
                    )));
                }
                target.absent = true;
                Ok(Found::Nothing)
            }
            1 => {
                let (element, description) = found.remove(0);
                target.absent = false;
                target.report.locator = Some(self.vars.mask(&element.locator_text()));
                target.report.description = Some(self.vars.mask(&description));
                self.generate(line, target, &element, client, state).await;
                Ok(Found::One(target.element_locator(element.element_ref())))
            }
            _ => {
                let candidates = found
                    .iter()
                    .map(|(element, description)| {
                        self.vars
                            .mask(&format!("{} ({description})", element.locator_text()))
                    })
                    .collect();
                Err(StepEnd::Failed(strictness(target, candidates)))
            }
        }
    }

    /// Generates the strict locator that the cache stores for the element
    /// the model found (SPEC 12.1).
    async fn generate(
        &mut self,
        line: &mut ActLine<'_>,
        target: &mut AiTarget,
        element: &Target,
        client: &mut ShimClient,
        state: &mut EntryState,
    ) {
        match self.generated_locator(line, element, client, state).await {
            Ok(locator) => {
                target.report.locator = Some(self.vars.mask(&locator));
                target.found = Some((locator, element.fingerprint()));
                target.uncacheable = None;
            }
            Err(reason) => {
                target.found = None;
                target.uncacheable = Some(reason);
            }
        }
    }

    /// The generated locator text of a snapshot element, or the warning
    /// code and reason when the cache cannot hold it.
    pub(super) async fn generated_locator(
        &mut self,
        line: &mut ActLine<'_>,
        element: &Target,
        client: &mut ShimClient,
        state: &mut EntryState,
    ) -> Result<String, (&'static str, String)> {
        let fingerprint = element.fingerprint();
        let command = StepCommand::GenerateLocator {
            element: element.element_ref().to_owned(),
            role:    fingerprint.role,
            name:    fingerprint.name,
        };
        let result = self
            .act_shim_call(line, command, client, state)
            .await
            .map_err(|_| ("cache-unstable", "the locator generator failed".to_owned()))?;
        match serde_json::from_value::<GeneratedLocator>(result) {
            Ok(GeneratedLocator::Locator { locator }) => {
                if let Some(role) =
                    locator
                        .segments
                        .iter()
                        .find_map(|segment| match &segment.kind {
                            SegmentKind::Role { role, .. } if !is_role(role) => Some(role),
                            _ => None,
                        })
                {
                    return Err((
                        "cache-unstable",
                        format!("the role `{role}` has no locator prefix"),
                    ));
                }
                let text = render_snapshot_target(&locator);
                if self.vars.mask(&text) != text {
                    return Err((
                        "cache-secret",
                        "its locator holds a masked value".to_owned(),
                    ));
                }
                Ok(text)
            }
            Ok(GeneratedLocator::Unstable { reason }) => Err(("cache-unstable", reason)),
            Err(_) => Err((
                "cache-unstable",
                "malformed generateLocator result from the shim".to_owned(),
            )),
        }
    }

    /// Finds the target's element, asking the model at most once every 2
    /// seconds within the line's time (SPEC 6.3). In an absence check, no
    /// element ends the wait.
    async fn resolve_target(
        &mut self,
        line: &mut ActLine<'_>,
        target: &mut AiTarget,
        usage: &mut PlanUsage,
        client: &mut ShimClient,
        state: &mut EntryState,
    ) -> Result<Found, StepEnd> {
        loop {
            if let Some(asked) = target.last_ask {
                let next = asked + ASK_INTERVAL;
                if next >= line.deadline {
                    return Err(Self::not_found(line, target));
                }
                sleep(next.saturating_duration_since(Instant::now())).await;
            }
            if line.remaining_ms() == 0 {
                return Err(Self::not_found(line, target));
            }
            match self.find_target(line, target, usage, client, state).await? {
                Found::Nothing if !target.absence_ok => {}
                found => return Ok(found),
            }
        }
    }

    /// The failure when no element matched in the line's time.
    fn not_found(line: &ActLine<'_>, target: &AiTarget) -> StepEnd {
        if line.budget.entry_capped {
            return StepEnd::Failed(entry_timeout_error(line.budget.entry_budget_ms));
        }
        StepEnd::Failed(StepError {
            code: "timeout".to_owned(),
            message: format!(
                "timeout: no element matched {} within {}ms",
                target.report.target, line.budget.timeout_ms
            ),
            ..StepError::default()
        })
    }

    /// Runs an action or a state check whose locators include `ai:`
    /// targets: resolve each target, then run the line on the elements. A
    /// command whose element the page replaced resolves once more.
    pub(super) async fn run_ai_line(
        &mut self,
        node: StepNode<'_>,
        implicit_response: Option<&str>,
        title: &str,
        budget: ActBudget,
        client: &mut ShimClient,
        state: &mut EntryState,
    ) -> AiLineRun {
        let mut spend = AiSpend::default();
        let mut extract = None;
        let mut judge = None;
        let mut targets: Vec<AiTarget> = ai_locators(node)
            .into_iter()
            .map(|(locator, absence_ok)| self.ai_target(node, locator, absence_ok))
            .collect();
        let deadline = Instant::now() + Duration::from_millis(budget.timeout_ms);
        let mut retry_left = true;
        let mut act = None;
        let end = loop {
            let mut line = ActLine {
                node,
                title,
                deadline,
                budget,
                entry_start: state.steps.is_empty(),
                what: "the step",
            };
            let mut resolved = Vec::with_capacity(targets.len());
            let mut absent = false;
            let mut failed = None;
            for target in &mut targets {
                match self
                    .resolve_target(&mut line, target, &mut spend.usage, client, state)
                    .await
                {
                    Ok(Found::One(locator)) => resolved.push(locator),
                    Ok(Found::Nothing) => absent = true,
                    Err(end) => {
                        failed = Some(end);
                        break;
                    }
                }
            }
            if let Some(end) = failed {
                break end;
            }
            if absent {
                // Only an absence check accepts no element (SPEC 6.3).
                break StepEnd::Passed;
            }
            let replaced = replace_targets(node, &resolved);
            let replaced_node = replaced.node();
            let prepared = match self.prepare_step(replaced_node, implicit_response) {
                Ok(prepared) => prepared,
                Err(error) => {
                    break StepEnd::Failed(StepError {
                        code: error.code().to_owned(),
                        message: self.vars.mask(&error.to_string()),
                        ..StepError::default()
                    });
                }
            };
            match prepared {
                PreparedStep::Command(command) | PreparedStep::Snapshot { command, .. } => {
                    let mut line = ActLine {
                        node: replaced_node,
                        title,
                        deadline,
                        budget,
                        entry_start: state.steps.is_empty(),
                        what: "the step",
                    };
                    match self.act_shim_call(&mut line, command, client, state).await {
                        Ok(_) => break StepEnd::Passed,
                        Err(failure) if failure.stale_ref() && retry_left => {
                            // The page replaced the element: find it again.
                            retry_left = false;
                            for target in &mut targets {
                                target.last_ask = None;
                                target.cache_tried = true;
                            }
                        }
                        Err(failure) => break failure.end,
                    }
                }
                PreparedStep::Act { instruction, scope } => {
                    let remaining = ActBudget {
                        timeout_ms: u64::try_from(
                            deadline
                                .saturating_duration_since(Instant::now())
                                .as_millis(),
                        )
                        .unwrap_or(u64::MAX),
                        ..budget
                    };
                    let (end, report, warnings) = self
                        .run_act(
                            replaced_node,
                            &instruction,
                            scope,
                            title,
                            remaining,
                            client,
                            state,
                        )
                        .await;
                    act = report;
                    spend.warnings.extend(warnings);
                    break end;
                }
                PreparedStep::Extract(plan) => {
                    let remaining = ActBudget {
                        timeout_ms: u64::try_from(
                            deadline
                                .saturating_duration_since(Instant::now())
                                .as_millis(),
                        )
                        .unwrap_or(u64::MAX),
                        ..budget
                    };
                    let (end, report) = self
                        .run_extract(replaced_node, plan, title, remaining, client, state)
                        .await;
                    extract = report;
                    break end;
                }
                PreparedStep::Judge(plan) => {
                    let remaining = ActBudget {
                        timeout_ms: u64::try_from(
                            deadline
                                .saturating_duration_since(Instant::now())
                                .as_millis(),
                        )
                        .unwrap_or(u64::MAX),
                        ..budget
                    };
                    let (end, report, warnings) = self
                        .run_judge(replaced_node, plan, title, remaining, client, state)
                        .await;
                    judge = report;
                    spend.warnings.extend(warnings);
                    break end;
                }
                PreparedStep::Check(_) | PreparedStep::Capture(_) => {
                    unreachable!("checks with a subject resolve their targets while they read")
                }
                PreparedStep::Goal(_) => unreachable!("GOAL has no locator"),
            }
        };
        let passed = matches!(end, StepEnd::Passed);
        for target in targets {
            if passed {
                self.finish_target(node, target, &mut spend);
            } else {
                spend.targets.push(target.report);
            }
        }
        AiLineRun {
            end,
            act,
            extract,
            judge,
            spend,
        }
    }
}

/// What a line with `ai:` targets did.
pub(super) struct AiLineRun {
    pub(super) end:     StepEnd,
    pub(super) act:     Option<ActReport>,
    pub(super) extract: Option<ExtractReport>,
    pub(super) judge:   Option<JudgeReport>,
    pub(super) spend:   AiSpend,
}

/// The `ai:` locators that a line resolves before it runs, each with
/// whether no element passes: an action's targets and scope, and a state
/// check's locator. Checks and captures with a subject resolve while they
/// read, so they are not listed.
pub(super) fn ai_locators(node: StepNode<'_>) -> Vec<(&Locator, bool)> {
    match node {
        StepNode::Action(action) => action
            .kind
            .locators()
            .into_iter()
            .filter(|locator| locator.ai_description().is_some())
            .map(|locator| (locator, false))
            .collect(),
        StepNode::Assert(ast::Assert {
            body: ast::AssertBody::ElementState { locator, state },
            ..
        }) if locator.ai_description().is_some() => {
            vec![(locator, *state == ast::StateCheck::Hidden)]
        }
        StepNode::Judge(ast::Judge {
            scope: Some(scope), ..
        }) if scope.ai_description().is_some() => vec![(scope, false)],
        StepNode::Assert(_) | StepNode::Judge(_) | StepNode::Capture(_) | StepNode::Page(_) => {
            Vec::new()
        }
    }
}

/// A line with its `ai:` targets replaced by the elements they resolved to.
enum Replaced {
    Action(ast::Action),
    Assert(ast::Assert),
    Judge(ast::Judge),
}

impl Replaced {
    fn node(&self) -> StepNode<'_> {
        match self {
            Self::Action(action) => StepNode::Action(action),
            Self::Assert(assert) => StepNode::Assert(assert),
            Self::Judge(judge) => StepNode::Judge(judge),
        }
    }
}

/// Replaces the line's `ai:` locators, in order, with `resolved`.
fn replace_targets(node: StepNode<'_>, resolved: &[Locator]) -> Replaced {
    let mut resolved = resolved.iter();
    let mut next = |locator: &mut Locator| {
        if locator.ai_description().is_some()
            && let Some(replacement) = resolved.next()
        {
            *locator = replacement.clone();
        }
    };
    match node {
        StepNode::Action(action) => {
            let mut action = action.clone();
            for locator in action.kind.locators_mut() {
                next(locator);
            }
            Replaced::Action(action)
        }
        StepNode::Assert(assert) => {
            let mut assert = assert.clone();
            if let ast::AssertBody::ElementState { locator, .. } = &mut assert.body {
                next(locator);
            }
            Replaced::Assert(assert)
        }
        StepNode::Judge(judge) => {
            let mut judge = judge.clone();
            if let Some(scope) = &mut judge.scope {
                next(scope);
            }
            Replaced::Judge(judge)
        }
        StepNode::Capture(_) | StepNode::Page(_) => {
            unreachable!("only actions, state checks, and JUDGE resolve before they run")
        }
    }
}
