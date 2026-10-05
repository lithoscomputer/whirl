//! Lint rules for parsed `.whirl` files (SPEC sections 14, 16).
//!
//! [`lint_file`] reports diagnostics for one file. Errors stop the
//! invocation with exit code 2 like parse errors; warnings do not change
//! the exit code (SPEC 16). The CLI layer owns both mappings.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

use crate::check::{Number, PredicateKind, StaticType, ValueType, is_bytes_literal_shape};
use crate::lang::ast::snapshot::SnapshotOption;
use crate::lang::ast::{
    Action, ActionKind, Assert, AssertBody, Capture, CheckLine, CheckStep, Entry, Extractor, File,
    FileOption, FilterArg, FilterSpec, Ident, Locator, MockResponse, Operand, OptionValue,
    PageCheck, PredicateSpec, RequestField, ResponseField, SegmentKind, Span, StateCheck, Subject,
    Value, ValueSegment, chain_type,
};
use crate::lang::schema;

/// How serious a lint diagnostic is: an [`Severity::Error`] fails
/// `whirl check` and `whirl` runs with exit code 2; a
/// [`Severity::Warning`] is reported without changing the exit code.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Severity {
    Error,
    Warning,
}

/// One lint diagnostic, located like a parse error (SPEC 16).
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct Lint {
    pub(crate) code:     &'static str,
    pub(crate) severity: Severity,
    pub(crate) path:     PathBuf,
    /// 1-based line of the offending token.
    pub(crate) line:     u32,
    /// 1-based character column of the offending token.
    pub(crate) column:   u32,
    /// Length of the offending token in characters.
    pub(crate) len:      u32,
    pub(crate) message:  String,
}

/// Lints a parsed file. `external_uses` names captures read by dependent
/// files through `setup:`; those captures count as used (SPEC 16).
pub(crate) fn lint_file_with(file: &File, external_uses: &HashSet<String>) -> Vec<Lint> {
    let mut lints = Vec::new();
    duplicate_artifact_names(file, &mut lints);
    window_names(file, &mut lints);
    response_names(file, &mut lints);
    unasserted_http_status(file, &mut lints);
    unused_captures(file, external_uses, &mut lints);
    setup_option_rules(file, &mut lints);
    redundant_presence_counts(file, &mut lints);
    filter_types(file, &mut lints);
    ai_counts(file, &mut lints);
    extract_rules(file, &mut lints);
    judge_alone(file, &mut lints);
    goal_unchecked(file, &mut lints);
    lints.sort_by_key(|lint| (lint.line, lint.column));
    lints
}

/// The capture names a file reads as `{{setup.name}}`.
pub(crate) fn setup_capture_uses(file: &File) -> HashSet<String> {
    collect_setup_refs(file)
        .into_iter()
        .map(|var_ref| var_ref.name.to_owned())
        .collect()
}

/// Lints a file against its parsed `setup` flow (SPEC 12, 16): the setup
/// flow may not name a setup of its own, and every `{{setup.name}}` the
/// file reads must be a capture the setup flow takes.
pub(crate) fn lint_setup_refs(file: &File, setup: &File) -> Vec<Lint> {
    let mut lints = Vec::new();
    if let (Some(line), Some(nested)) = (file.setup_option(), setup.setup_option()) {
        let message = format!(
            "setup flow '{}' names its own setup on line {}; only one level is allowed",
            setup.path.display(),
            nested.line
        );
        lints.push(lint_at(
            file,
            Severity::Error,
            "nested-setup",
            line.span,
            message,
        ));
    }
    let captured: HashSet<&str> = setup
        .entries
        .iter()
        .flat_map(Entry::captures)
        .map(|capture| capture.name.text.as_str())
        .collect();
    for var_ref in collect_setup_refs(file) {
        if !captured.contains(var_ref.name) {
            let message = format!(
                "setup flow '{}' has no capture `{}`",
                setup.path.display(),
                var_ref.name
            );
            lints.push(lint_at(
                file,
                Severity::Error,
                "unknown-setup-capture",
                var_ref.span,
                message,
            ));
        }
    }
    lints.sort_by_key(|lint| (lint.line, lint.column));
    lints
}

/// What the model catalog says about one model.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ModelFacts {
    Unknown,
    Known {
        /// Whether the model accepts images; `None` when the catalog does
        /// not say.
        images: Option<bool>,
    },
}

/// Model rules (SPEC 5, 7.4, 9.8): a file that uses the model needs a
/// `model` option, a literal model must be one the catalog knows, and a
/// file that uses `JUDGE` needs a model that accepts images. The caller
/// looks the model up, because the catalog depends on the environment
/// (SPEC 13).
pub(crate) fn lint_act(file: &File, facts: impl Fn(&str) -> ModelFacts) -> Vec<Lint> {
    if let Some(line) = file.model_option() {
        let FileOption::Model(value) = &line.option else {
            return Vec::new();
        };
        let Some(model) = value.as_literal() else {
            return Vec::new();
        };
        let lint = match facts(&model) {
            ModelFacts::Unknown => lint_at(
                file,
                Severity::Error,
                "unknown-model",
                value.span,
                format!(
                    "unknown model `{model}`; use a provider/model name such as anthropic/claude-sonnet-5"
                ),
            ),
            ModelFacts::Known {
                images: Some(false),
            } if file.uses_judge() => lint_at(
                file,
                Severity::Error,
                "judge-without-images",
                value.span,
                format!("JUDGE sends a screenshot, and the model `{model}` does not accept images"),
            ),
            ModelFacts::Known { images: None } if file.uses_judge() => lint_at(
                file,
                Severity::Warning,
                "judge-images-unknown",
                value.span,
                format!(
                    "JUDGE sends a screenshot, and the catalog does not say whether the model `{model}` accepts images"
                ),
            ),
            ModelFacts::Known { .. } => return Vec::new(),
        };
        return vec![lint];
    }
    let act = file
        .entries
        .iter()
        .flat_map(|entry| {
            let actions = entry.actions.iter().filter_map(|action| match action.kind {
                ActionKind::Act { .. } => Some((action.span, "ACT")),
                ActionKind::Goal { .. } => Some((action.span, "GOAL")),
                ActionKind::Extract { .. } => Some((action.span, "EXTRACT")),
                _ => None,
            });
            actions.chain(entry.judges().map(|judge| (judge.span, "JUDGE")))
        })
        .next();
    let target = file.locator_uses().into_iter().find_map(|used| {
        let segment = used.locator.segments.last()?;
        used.locator
            .ai_description()
            .map(|_| (segment.span, "`ai:`"))
    });
    let first = match (act, target) {
        (Some(act), Some(target)) => Some(
            if (act.0.line, act.0.column) <= (target.0.line, target.0.column) {
                act
            } else {
                target
            },
        ),
        (act, target) => act.or(target),
    };
    first
        .map(|(span, what)| {
            lint_at(
                file,
                Severity::Error,
                "act-without-model",
                span,
                format!("{what} needs a `model` option naming the language model to ask"),
            )
        })
        .into_iter()
        .collect()
}

/// The model decides when a `GOAL` is done, so the `GOAL` must be the last
/// action of an entry with an `ASSERT` that checks the result (SPEC 7.7).
fn goal_unchecked(file: &File, lints: &mut Vec<Lint>) {
    for entry in &file.entries {
        let last = entry.actions.len().saturating_sub(1);
        let checked = entry.asserts().next().is_some();
        for (index, action) in entry.actions.iter().enumerate() {
            if matches!(action.kind, ActionKind::Goal { .. }) && (index != last || !checked) {
                lints.push(lint_at(
                    file,
                    Severity::Error,
                    "goal-unchecked",
                    action.span,
                    "an ASSERT must follow GOAL, before any other action, to check that the goal was reached".to_owned(),
                ));
            }
        }
    }
}

/// `JUDGE` does not wait for the state it judges, so an entry needs an
/// `ASSERT` that waits (SPEC 9.8).
fn judge_alone(file: &File, lints: &mut Vec<Lint>) {
    for entry in &file.entries {
        if entry.asserts().next().is_some() {
            continue;
        }
        if let Some(judge) = entry.judges().next() {
            lints.push(lint_at(
                file,
                Severity::Warning,
                "judge-alone",
                judge.span,
                "JUDGE does not retry; add an ASSERT before it that waits for the state the claim describes".to_owned(),
            ));
        }
    }
}

/// `EXTRACT` rules (SPEC 7.6): unique names read after their line, a
/// schema in the subset, and a warning for a line that reads the page
/// right after an interaction.
fn extract_rules(file: &File, lints: &mut Vec<Lint>) {
    let mut names = HashSet::new();
    for entry in &file.entries {
        for (index, action) in entry.actions.iter().enumerate() {
            let ActionKind::Extract { name, schema, .. } = &action.kind else {
                continue;
            };
            if !names.insert(name.text.as_str()) {
                lints.push(lint_at(
                    file,
                    Severity::Error,
                    "duplicate-extract",
                    name.span,
                    format!("EXTRACT `{}` is already named", name.text),
                ));
            }
            if let Some(schema) = schema
                && let Some(problem) = schema::unsupported(&schema.json())
            {
                lints.push(lint_at(
                    file,
                    Severity::Error,
                    "extract-schema-unsupported",
                    Span {
                        line:   schema.line,
                        column: 1,
                        len:    1,
                    },
                    format!("the EXTRACT schema is outside the supported subset: {problem}"),
                ));
            }
            let interacts = index
                .checked_sub(1)
                .and_then(|previous| entry.actions.get(previous))
                .is_some_and(|previous| interacts(&previous.kind));
            if interacts {
                lints.push(lint_at(
                    file,
                    Severity::Warning,
                    "extract-unsettled",
                    action.span,
                    "EXTRACT runs once, right after an interaction; add an ASSERT that waits for the page first"
                        .to_owned(),
                ));
            }
        }
        for check in &entry.checks {
            let subject = match check {
                CheckStep::Assert(Assert {
                    body: AssertBody::Check(line),
                    ..
                }) => &line.subject,
                CheckStep::Capture(capture) => &capture.subject,
                CheckStep::Assert(_) | CheckStep::Judge(_) => continue,
            };
            if let Subject::Extract { name, .. } = subject
                && !names.contains(name.text.as_str())
            {
                lints.push(lint_at(
                    file,
                    Severity::Error,
                    "unknown-extract",
                    name.span,
                    format!(
                        "unknown EXTRACT `{}`; name it with EXTRACT first",
                        name.text
                    ),
                ));
            }
        }
    }
}

/// True for an action that changes what the page shows.
fn interacts(kind: &ActionKind) -> bool {
    matches!(
        kind,
        ActionKind::Click { .. }
            | ActionKind::Dblclick { .. }
            | ActionKind::Fill { .. }
            | ActionKind::Type { .. }
            | ActionKind::Press { .. }
            | ActionKind::Check { .. }
            | ActionKind::Uncheck { .. }
            | ActionKind::Select { .. }
            | ActionKind::Hover { .. }
            | ActionKind::Drag { .. }
            | ActionKind::Scroll { .. }
            | ActionKind::ScrollIntoView { .. }
            | ActionKind::Upload { .. }
            | ActionKind::Drop { .. }
            | ActionKind::Act { .. }
            | ActionKind::Goal { .. }
            | ActionKind::Eval { .. }
    )
}

/// `ai:` names one element, so it cannot be counted (SPEC 6.3).
fn ai_counts(file: &File, lints: &mut Vec<Lint>) {
    for used in file.locator_uses() {
        if used.count
            && used.locator.ai_description().is_some()
            && let Some(segment) = used.locator.segments.last()
        {
            lints.push(lint_at(
                file,
                Severity::Error,
                "ai-count",
                segment.span,
                "`ai:` names one element and cannot be counted; count a locator without `ai:`"
                    .to_owned(),
            ));
        }
    }
}

/// A plain `LOCATOR count OP N` check: no filters, no `not`, and a
/// literal integer `N`.
fn count_check(assert: &Assert) -> Option<(&Locator, PredicateKind, i64)> {
    let AssertBody::Check(CheckLine {
        subject:
            Subject::Element {
                locator,
                extractor: Extractor::Count,
            },
        filters,
        negated,
        predicate:
            PredicateSpec::Compare {
                kind,
                expected: Operand::Value(value),
            },
    }) = &assert.body
    else {
        return None;
    };
    if !filters.is_empty() {
        return None;
    }
    let count = Number::parse(&value.as_literal()?)?.to_i64()?;
    // `not` flips the comparison: `not < 1` means `>= 1` (SPEC 9.4).
    let kind = if *negated {
        match kind {
            PredicateKind::Eq => PredicateKind::Ne,
            PredicateKind::Ne => PredicateKind::Eq,
            PredicateKind::Gt => PredicateKind::Le,
            PredicateKind::Ge => PredicateKind::Lt,
            PredicateKind::Lt => PredicateKind::Ge,
            PredicateKind::Le => PredicateKind::Gt,
            _ => return None,
        }
    } else {
        *kind
    };
    Some((locator, kind, count))
}

/// Warns about a `count >= 1` (or `count > 0`, `count != 0`) assert
/// directly followed by a check on the same locator that requires
/// presence (SPEC 16). Checks accepting zero matches preserve the wait.
fn redundant_presence_counts(file: &File, lints: &mut Vec<Lint>) {
    for entry in &file.entries {
        for pair in entry.checks.windows(2) {
            let [CheckStep::Assert(first), CheckStep::Assert(second)] = pair else {
                continue;
            };
            let pair = [first, second];
            let Some((locator, kind, count)) = count_check(pair[0]) else {
                continue;
            };
            let asserts_presence = matches!(
                (kind, count),
                (PredicateKind::Ge, 1) | (PredicateKind::Gt | PredicateKind::Ne, 0)
            );
            if !asserts_presence {
                continue;
            }
            let next = if let Some((locator, kind, count)) = count_check(pair[1]) {
                let accepts_zero = match kind {
                    PredicateKind::Eq => count == 0,
                    PredicateKind::Ne => count != 0,
                    PredicateKind::Gt => count < 0,
                    PredicateKind::Ge => count <= 0,
                    PredicateKind::Lt => count > 0,
                    PredicateKind::Le => count >= 0,
                    _ => false,
                };
                if accepts_zero {
                    continue;
                }
                locator
            } else {
                match &pair[1].body {
                    AssertBody::ElementState {
                        state: StateCheck::Hidden,
                        ..
                    } => continue,
                    AssertBody::ElementState { locator, .. } => locator,
                    // `not exists` accepts a missing element (SPEC 9.7).
                    AssertBody::Check(CheckLine {
                        subject: Subject::Element { locator, extractor },
                        negated,
                        predicate,
                        ..
                    }) if *extractor != Extractor::Count
                        && !(*negated && predicate.kind() == PredicateKind::Exists) =>
                    {
                        locator
                    }
                    _ => continue,
                }
            };
            if locator_key(locator) == locator_key(next) {
                lints.push(lint_at(
                    file,
                    Severity::Warning,
                    "redundant-presence",
                    pair[0].span,
                    format!(
                        "this presence check is redundant; the check on line {} already waits for the element",
                        pair[1].line
                    ),
                ));
            }
        }
    }
}

/// Reports a check whose subject, filters, and predicate cannot work
/// together (SPEC 16): a filter that cannot take its input, or a
/// predicate that cannot test the value.
fn filter_types(file: &File, lints: &mut Vec<Lint>) {
    for entry in &file.entries {
        for assert in entry.asserts() {
            let AssertBody::Check(check) = &assert.body else {
                continue;
            };
            match chain_type(&check.subject, &check.filters) {
                Err((filter, input)) => lints.push(filter_type_lint(file, filter, &input)),
                Ok(value_type) => {
                    let kind = check.predicate.kind();
                    let message = if kind.accepts(&value_type) {
                        expected_type_mismatch(check, &value_type)
                    } else {
                        Some(format!("`{}` cannot test a {value_type}", kind.name()))
                    };
                    if let Some(message) = message {
                        lints.push(lint_at(
                            file,
                            Severity::Error,
                            "filter-type",
                            assert.span,
                            message,
                        ));
                    }
                }
            }
        }
        for capture in entry.captures() {
            if let Err((filter, input)) = chain_type(&capture.subject, &capture.filters) {
                lints.push(filter_type_lint(file, filter, &input));
            }
        }
    }
}

/// A literal expected value whose type can never work with the value's
/// type (SPEC 9.4, 9.6): `url toDate "%Y" > 3`, or `status == "200"`.
/// `!=` and `not ==` across types pass, so they are fine.
fn expected_type_mismatch(check: &CheckLine, value_type: &StaticType) -> Option<String> {
    let PredicateSpec::Compare { kind, expected } = &check.predicate else {
        return None;
    };
    let value = match value_type {
        StaticType::Known(ValueType::String) | StaticType::Any => return None,
        StaticType::Known(known) => *known,
        StaticType::ListOf(_) => ValueType::List,
    };
    let expected_type = literal_type(expected)?;
    let fails = match kind {
        PredicateKind::Eq => !check.negated && value != expected_type,
        PredicateKind::Gt | PredicateKind::Ge | PredicateKind::Lt | PredicateKind::Le => {
            value == ValueType::Date || expected_type != ValueType::Number
        }
        PredicateKind::StartsWith | PredicateKind::EndsWith | PredicateKind::Contains => {
            value == ValueType::Bytes && expected_type != ValueType::Bytes
        }
        _ => false,
    };
    fails.then(|| {
        format!(
            "`{}` cannot compare {} values with a {} literal",
            kind.name(),
            value.name(),
            expected_type.name()
        )
    })
}

/// The type of a literal expected value in a typed check (SPEC 9.6), or
/// `None` when a variable decides it.
fn literal_type(expected: &Operand) -> Option<ValueType> {
    match expected {
        Operand::Json(literal) => Some(if literal.text.starts_with('[') {
            ValueType::List
        } else {
            ValueType::Object
        }),
        Operand::Value(value) if value.quoted => Some(ValueType::String),
        Operand::Value(value) => {
            let text = value.as_literal()?;
            Some(match text.as_str() {
                "true" | "false" => ValueType::Boolean,
                "null" => ValueType::Null,
                _ if Number::parse(&text).is_some() => ValueType::Number,
                _ if is_bytes_literal_shape(&text) => ValueType::Bytes,
                _ => ValueType::String,
            })
        }
    }
}

fn filter_type_lint(file: &File, filter: &FilterSpec, input: &StaticType) -> Lint {
    lint_at(
        file,
        Severity::Error,
        "filter-type",
        filter.span,
        format!(
            "`{}` cannot take a {input}",
            filter.kind.name().trim_end_matches(':')
        ),
    )
}

/// A span-free rendering of a locator, so two lines that spell the same
/// locator compare equal.
fn locator_key(locator: &Locator) -> String {
    locator
        .segments
        .iter()
        .map(|segment| match &segment.kind {
            SegmentKind::Role {
                substring,
                role,
                name,
            } => format!(
                "role{}:{role} {}",
                if *substring { "~" } else { "" },
                name.as_ref().map(value_key).unwrap_or_default()
            ),
            SegmentKind::TextEngine {
                prefix,
                substring,
                value,
            } => format!(
                "{prefix:?}{}:{}",
                if *substring { "~" } else { "" },
                value_key(value)
            ),
            SegmentKind::TestId(value) => format!("testid:{}", value_key(value)),
            SegmentKind::Css(value) => format!("css:{}", value_key(value)),
            SegmentKind::Frame(value) => format!("frame:{}", value_key(value)),
            SegmentKind::Nth(index) => format!("nth:{index}"),
            SegmentKind::Default(value) => format!("default:{}", value_key(value)),
            SegmentKind::Ai(value) => format!("ai:{}", value_key(value)),
            SegmentKind::Ref(element) => format!("ref:{element}"),
        })
        .collect::<Vec<_>>()
        .join(" >> ")
}

fn value_key(value: &Value) -> String {
    value
        .segments
        .iter()
        .map(|segment| match segment {
            ValueSegment::Literal(text) => text.clone(),
            ValueSegment::Var(name) => format!("{{{{{name}}}}}"),
            ValueSegment::EnvVar(name) => format!("{{{{env.{name}}}}}"),
            ValueSegment::SetupVar(name) => format!("{{{{setup.{name}}}}}"),
        })
        .collect()
}

/// Rules for the `setup` option itself (SPEC 5, 11): it cannot be
/// combined with `storage`, its path must be literal, and `{{setup.name}}`
/// needs a `setup` option to read from.
fn setup_option_rules(file: &File, lints: &mut Vec<Lint>) {
    let setup = file.setup_option();
    if let Some(line) = setup {
        if let FileOption::Setup(value) = &line.option
            && !value.is_literal()
        {
            lints.push(lint_at(
                file,
                Severity::Error,
                "interpolated-setup",
                value.span,
                "the setup path must be literal; it is resolved before any variable exists"
                    .to_owned(),
            ));
        }
        if let Some(storage) = file.storage_option() {
            lints.push(lint_at(
                file,
                Severity::Error,
                "conflicting-storage",
                storage.span,
                "storage and setup both set the starting state; use one".to_owned(),
            ));
        }
    } else {
        for var_ref in collect_setup_refs(file) {
            let message = format!(
                "`{{{{setup.{}}}}}` needs a setup option naming the flow that captures it",
                var_ref.name
            );
            lints.push(lint_at(
                file,
                Severity::Error,
                "missing-setup",
                var_ref.span,
                message,
            ));
        }
    }
}

fn lint_at(
    file: &File,
    severity: Severity,
    code: &'static str,
    span: Span,
    message: String,
) -> Lint {
    Lint {
        code,
        severity,
        path: file.path.clone(),
        line: span.line,
        column: span.column,
        len: span.len,
        message,
    }
}

/// Duplicate `SCREENSHOT` or `SNAPSHOT` names within one flow are a lint
/// error (SPEC 14): artifacts inside a flow's directory are named by the
/// given name, so a duplicate would overwrite an earlier artifact.
/// `SCREENSHOT` and `SNAPSHOT` names are separate namespaces, because
/// their artifact file names never collide.
fn duplicate_artifact_names(file: &File, lints: &mut Vec<Lint>) {
    let mut first_lines: HashMap<(&'static str, &str), u32> = HashMap::new();
    let names = file.entries.iter().flat_map(|entry| {
        entry
            .actions
            .iter()
            .filter_map(|action| match &action.kind {
                ActionKind::Screenshot { name } => Some(("SCREENSHOT", name)),
                ActionKind::Snapshot { name, .. } => Some(("SNAPSHOT", name)),
                _ => None,
            })
    });
    for (kind, name) in names {
        match first_lines.get(&(kind, name.text.as_str())) {
            Some(first_line) => {
                let message = format!(
                    "duplicate {kind} name `{}`; first used on line {first_line}",
                    name.text
                );
                lints.push(lint_at(
                    file,
                    Severity::Error,
                    "duplicate-artifact",
                    name.span,
                    message,
                ));
            }
            None => {
                first_lines.insert((kind, name.text.as_str()), name.span.line);
            }
        }
    }
}

/// Which namespace a reference reads (SPEC 11).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RefKind {
    /// `{{name}}`: a capture or a command-line variable.
    Var,
    /// `{{setup.name}}`: a capture of the setup flow.
    Setup,
}

/// A `{{name}}` or `{{setup.name}}` variable reference and where it
/// appears.
struct VarRef<'a> {
    kind: RefKind,
    name: &'a str,
    line: u32,
    span: Span,
}

/// Warns about captures no later line uses (SPEC 16). A use is a
/// `{{name}}` reference in a value on a later line, up to and including
/// the line of the next capture that overwrites the name: that capture's
/// own source still reads the old value, but overwriting is not a use.
fn unused_captures(file: &File, external_uses: &HashSet<String>, lints: &mut Vec<Lint>) {
    let refs: Vec<VarRef<'_>> = collect_var_refs(file)
        .into_iter()
        .filter(|var_ref| var_ref.kind == RefKind::Var)
        .collect();
    let captures: Vec<&Capture> = file.entries.iter().flat_map(Entry::captures).collect();
    for (index, capture) in captures.iter().enumerate() {
        let overwrite_line = captures[index + 1..]
            .iter()
            .find(|later| later.name.text == capture.name.text)
            .map(|later| later.line);
        let used = external_uses.contains(&capture.name.text)
            || refs.iter().any(|var_ref| {
                var_ref.name == capture.name.text
                    && var_ref.line > capture.line
                    && overwrite_line.is_none_or(|line| var_ref.line <= line)
            });
        if !used {
            let message = format!("capture `{}` is never used", capture.name.text);
            lints.push(lint_at(
                file,
                Severity::Warning,
                "unused-capture",
                capture.name.span,
                message,
            ));
        }
    }
}

/// Collects every `{{name}}` reference in the file's entries. Option
/// values resolve before any capture exists (SPEC 11), so they never use
/// a capture and are not collected.
fn collect_var_refs(file: &File) -> Vec<VarRef<'_>> {
    let mut refs = Vec::new();
    for entry in &file.entries {
        collect_entry_refs(entry, &mut refs);
    }
    refs
}

fn collect_entry_refs<'a>(entry: &'a Entry, refs: &mut Vec<VarRef<'a>>) {
    for action in &entry.actions {
        collect_action_refs(action, refs);
    }
    if let Some(page) = &entry.page
        && let PageCheck::Value(value) = &page.check
    {
        collect_value_refs(value, page.line, refs);
    }
    for assert in entry.asserts() {
        collect_assert_refs(assert, refs);
    }
    for capture in entry.captures() {
        collect_chain_refs(&capture.subject, &capture.filters, capture.line, refs);
    }
}

fn collect_snapshot_refs<'a>(option: &'a SnapshotOption, line: u32, refs: &mut Vec<VarRef<'a>>) {
    match option {
        SnapshotOption::Mask(Some(locator)) => collect_locator_refs(locator, line, refs),
        SnapshotOption::MaxDiff(OptionValue::Interpolated(value))
        | SnapshotOption::PixelThreshold(OptionValue::Interpolated(value)) => {
            collect_value_refs(value, line, refs);
        }
        _ => {}
    }
}

fn collect_action_refs<'a>(action: &'a Action, refs: &mut Vec<VarRef<'a>>) {
    let line = action.line;
    match &action.kind {
        ActionKind::Http {
            url, headers, body, ..
        }
        | ActionKind::Mock {
            url,
            response: MockResponse::Fulfill { headers, body, .. },
            ..
        } => {
            collect_value_refs(url, line, refs);
            for header in headers {
                collect_value_refs(&header.value, header.line, refs);
            }
            if let Some(body) = body {
                collect_value_refs(&body.value, body.line, refs);
            }
        }
        ActionKind::Response { url, .. }
        | ActionKind::Visit { url }
        | ActionKind::Mock {
            url,
            response: MockResponse::Failed,
            ..
        } => {
            collect_value_refs(url, line, refs);
        }
        ActionKind::Click { target, .. }
        | ActionKind::Dblclick { target }
        | ActionKind::Check { target }
        | ActionKind::Uncheck { target }
        | ActionKind::Hover { target }
        | ActionKind::ScrollIntoView { target } => collect_locator_refs(target, line, refs),
        ActionKind::Fill { target, value }
        | ActionKind::Type {
            target,
            text: value,
        }
        | ActionKind::Select {
            target,
            option: value,
        } => {
            collect_locator_refs(target, line, refs);
            collect_value_refs(value, line, refs);
        }
        ActionKind::Press { target, key } => {
            if let Some(target) = target {
                collect_locator_refs(target, line, refs);
            }
            collect_value_refs(key, line, refs);
        }
        ActionKind::Upload { target, path } | ActionKind::Drop { target, path } => {
            collect_locator_refs(target, line, refs);
            collect_value_refs(path, line, refs);
        }
        ActionKind::Drag { source, target } => {
            collect_locator_refs(source, line, refs);
            collect_locator_refs(target, line, refs);
        }
        ActionKind::Scroll { target, .. } => {
            if let Some(target) = target {
                collect_locator_refs(target, line, refs);
            }
        }
        ActionKind::Eval { script } | ActionKind::Goal { goal: script } => {
            collect_value_refs(script, line, refs);
        }
        ActionKind::Act { scope, instruction }
        | ActionKind::Extract {
            scope, instruction, ..
        } => {
            if let Some(scope) = scope {
                collect_locator_refs(scope, line, refs);
            }
            collect_value_refs(instruction, line, refs);
        }
        ActionKind::Store { key, value, .. } => {
            collect_value_refs(key, line, refs);
            collect_value_refs(value, line, refs);
        }
        ActionKind::Popup { .. }
        | ActionKind::Window { .. }
        | ActionKind::Close { .. }
        | ActionKind::Screenshot { .. } => {}
        ActionKind::Snapshot {
            target, options, ..
        } => {
            if let Some(target) = target {
                collect_locator_refs(target, line, refs);
            }
            for option in options {
                collect_snapshot_refs(&option.option, option.line, refs);
            }
        }
    }
}

fn collect_assert_refs<'a>(assert: &'a Assert, refs: &mut Vec<VarRef<'a>>) {
    let line = assert.line;
    match &assert.body {
        AssertBody::WindowClosed { .. } => {}
        AssertBody::ElementState { locator, .. } => collect_locator_refs(locator, line, refs),
        AssertBody::Check(check) => {
            collect_chain_refs(&check.subject, &check.filters, line, refs);
            if let PredicateSpec::Compare { expected, .. } = &check.predicate {
                match expected {
                    Operand::Value(value) => collect_value_refs(value, line, refs),
                    Operand::Json(literal) => collect_value_refs(&literal.value, line, refs),
                }
            }
        }
    }
}

fn collect_chain_refs<'a>(
    subject: &'a Subject,
    filters: &'a [FilterSpec],
    line: u32,
    refs: &mut Vec<VarRef<'a>>,
) {
    match subject {
        Subject::Element { locator, .. } => collect_locator_refs(locator, line, refs),
        Subject::Eval(script) => collect_value_refs(script, line, refs),
        Subject::Response { field, .. } => collect_response_field_refs(field, line, refs),
        Subject::Request { field, .. } => match field {
            RequestField::Header(value)
            | RequestField::Json(value)
            | RequestField::Xpath(value) => {
                collect_value_refs(value, line, refs);
            }
            RequestField::Method | RequestField::Url | RequestField::Body | RequestField::Bytes => {
            }
        },
        Subject::Url | Subject::Title | Subject::Extract { .. } => {}
    }
    for filter in filters {
        for arg in &filter.args {
            if let FilterArg::Value(value) = arg {
                collect_value_refs(value, line, refs);
            }
        }
    }
}

fn collect_locator_refs<'a>(locator: &'a Locator, line: u32, refs: &mut Vec<VarRef<'a>>) {
    for segment in &locator.segments {
        match &segment.kind {
            SegmentKind::Role { name, .. } => {
                if let Some(name) = name {
                    collect_value_refs(name, line, refs);
                }
            }
            SegmentKind::TextEngine { value, .. }
            | SegmentKind::TestId(value)
            | SegmentKind::Css(value)
            | SegmentKind::Frame(value)
            | SegmentKind::Default(value)
            | SegmentKind::Ai(value) => collect_value_refs(value, line, refs),
            SegmentKind::Nth(_) | SegmentKind::Ref(_) => {}
        }
    }
}

fn collect_value_refs<'a>(value: &'a Value, line: u32, refs: &mut Vec<VarRef<'a>>) {
    for segment in &value.segments {
        let (kind, name) = match segment {
            ValueSegment::Var(name) => (RefKind::Var, name),
            ValueSegment::SetupVar(name) => (RefKind::Setup, name),
            ValueSegment::Literal(_) | ValueSegment::EnvVar(_) => continue,
        };
        refs.push(VarRef {
            kind,
            name,
            line,
            span: value.span,
        });
    }
}

/// Every `{{setup.name}}` reference in the file, including option
/// values, in source order.
fn collect_setup_refs(file: &File) -> Vec<VarRef<'_>> {
    let mut refs = Vec::new();
    let mut values: Vec<(&Value, u32)> = Vec::new();
    for line in &file.options {
        match &line.option {
            FileOption::Snapshot(option) => collect_snapshot_refs(option, line.line, &mut refs),
            FileOption::Base(value)
            | FileOption::Storage(value)
            | FileOption::UserAgent(value)
            | FileOption::Setup(value)
            | FileOption::Model(value) => values.push((value, line.line)),
            FileOption::AllowHosts(globs) => {
                values.extend(globs.iter().map(|glob| (glob, line.line)));
            }
            FileOption::Browser(_)
            | FileOption::Viewport(_)
            | FileOption::StepTimeout(_)
            | FileOption::EntryTimeout(_)
            | FileOption::NavTimeout(_)
            | FileOption::Dialogs(_)
            | FileOption::ReducedMotion(_) => {}
        }
    }
    for (value, line) in values {
        collect_value_refs(value, line, &mut refs);
    }
    refs.extend(collect_var_refs(file));
    refs.retain(|var_ref| var_ref.kind == RefKind::Setup);
    refs.sort_by_key(|var_ref| (var_ref.line, var_ref.span.column));
    refs
}

fn collect_response_field_refs<'a>(
    field: &'a ResponseField,
    line: u32,
    refs: &mut Vec<VarRef<'a>>,
) {
    match field {
        ResponseField::Status
        | ResponseField::Location
        | ResponseField::Body
        | ResponseField::Bytes => {}
        ResponseField::Header(value) | ResponseField::Json(value) | ResponseField::Xpath(value) => {
            collect_value_refs(value, line, refs);
        }
    }
}

fn response_names(file: &File, lints: &mut Vec<Lint>) {
    let mut names = HashSet::new();
    for entry in &file.entries {
        for action in &entry.actions {
            if let ActionKind::Response { name, .. } = &action.kind
                && !names.insert(name.text.as_str())
            {
                lints.push(lint_at(
                    file,
                    Severity::Error,
                    "duplicate-response",
                    name.span,
                    format!("response `{}` is already named", name.text),
                ));
            }
        }
        let assertion_names = entry
            .asserts()
            .filter_map(|assertion| match &assertion.body {
                AssertBody::Check(check) => response_name_of(&check.subject),
                _ => None,
            });
        let capture_names = entry
            .captures()
            .filter_map(|capture| response_name_of(&capture.subject));
        for name in assertion_names.chain(capture_names) {
            if !names.contains(name.text.as_str()) {
                lints.push(lint_at(
                    file,
                    Severity::Error,
                    "unknown-response",
                    name.span,
                    format!(
                        "unknown response `{}`; name it with RESPONSE first",
                        name.text
                    ),
                ));
            }
        }
    }
}

/// The `RESPONSE` name a subject reads, if any: a response, or the
/// request that the response answered.
fn response_name_of(subject: &Subject) -> Option<&Ident> {
    match subject {
        Subject::Response {
            name: Some(name), ..
        }
        | Subject::Request { name, .. } => Some(name),
        _ => None,
    }
}

fn unasserted_http_status(file: &File, lints: &mut Vec<Lint>) {
    for entry in &file.entries {
        let Some(action) = entry.actions.first() else {
            continue;
        };
        if !matches!(action.kind, ActionKind::Http { .. })
            || entry.asserts().any(|assert| {
                matches!(
                    &assert.body,
                    AssertBody::Check(CheckLine {
                        subject: Subject::Response {
                            name:  None,
                            field: ResponseField::Status,
                        },
                        ..
                    })
                )
            })
        {
            continue;
        }
        lints.push(lint_at(
            file,
            Severity::Warning,
            "unasserted-http-status",
            action.span,
            "HTTP response status is not asserted; status codes do not fail implicitly".to_owned(),
        ));
    }
}

fn window_names(file: &File, lints: &mut Vec<Lint>) {
    let mut names = HashSet::from(["main"]);
    for entry in &file.entries {
        for action in &entry.actions {
            match &action.kind {
                ActionKind::Popup { name } => {
                    if !names.insert(&name.text) {
                        lints.push(lint_at(
                            file,
                            Severity::Error,
                            "duplicate-window",
                            name.span,
                            format!("window `{}` is already named", name.text),
                        ));
                    }
                }
                ActionKind::Window { name } | ActionKind::Close { name }
                    if !names.contains(name.text.as_str()) =>
                {
                    lints.push(lint_at(
                        file,
                        Severity::Error,
                        "unknown-window",
                        name.span,
                        format!("unknown window `{}`; name it with POPUP first", name.text),
                    ));
                }
                _ => {}
            }
        }
        for assertion in entry.asserts() {
            if let AssertBody::WindowClosed { name } = &assertion.body
                && !names.contains(name.text.as_str())
            {
                lints.push(lint_at(
                    file,
                    Severity::Error,
                    "unknown-window",
                    name.span,
                    format!("unknown window `{}`", name.text),
                ));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    fn lint_file(file: &File) -> Vec<Lint> {
        lint_file_with(file, &HashSet::new())
    }

    use std::path::Path;

    use super::*;
    use crate::lang::parse::parse_file;

    fn lint(source: &str) -> Vec<Lint> {
        let file = parse_file(Path::new("test.whirl"), source)
            .unwrap_or_else(|error| panic!("fixture should parse:\n{error}"));
        lint_file(&file)
    }

    #[test]
    fn snapshot_options_reference_captures_and_setup_values() {
        let lints = lint(
            "VISIT /\nCAPTURE mask: eval \"'a'\"\nCAPTURE limit: eval 1\nCAPTURE threshold: eval 0.2\nSNAPSHOT x\nsnapshot-mask: testid:{{mask}}\nsnapshot-max-diff: {{limit}}\nsnapshot-pixel-threshold: {{threshold}}\n",
        );
        assert!(lints.is_empty(), "{lints:?}");
        let lints = lint(
            "[Options]\nsnapshot-mask: testid:{{setup.mask}}\nsnapshot-max-diff: {{setup.limit}}\nsnapshot-pixel-threshold: {{setup.threshold}}\nVISIT /\nSNAPSHOT x\nsnapshot-mask: testid:{{setup.local}}\n",
        );
        assert_eq!(
            lints
                .iter()
                .filter(|lint| lint.code == "missing-setup")
                .count(),
            4
        );
    }

    #[test]
    fn snapshot_targets_reference_captures_and_setup_values() {
        let lints = lint("VISIT /\nCAPTURE row: eval \"'a'\"\nSNAPSHOT row testid:{{row}}\n");
        assert!(lints.is_empty(), "{lints:?}");
        let lints = lint("VISIT /\nSNAPSHOT row testid:{{setup.row}}\n");
        assert_eq!(lints.len(), 1, "{lints:?}");
        assert_eq!(lints[0].code, "missing-setup");
    }

    #[test]
    fn page_and_element_snapshots_share_one_name_space() {
        let lints = lint("VISIT /\nSNAPSHOT cart\nSNAPSHOT cart testid:cart\n");
        assert_eq!(lints.len(), 1, "{lints:?}");
        assert_eq!(lints[0].severity, Severity::Error);
        assert_eq!(lints[0].line, 3);
    }

    #[test]
    fn reports_duplicate_screenshot_names_as_errors() {
        let lints = lint("VISIT /\nSCREENSHOT overview\nSCREENSHOT overview\n");
        assert_eq!(lints.len(), 1);
        assert_eq!(lints[0].severity, Severity::Error);
        assert_eq!(lints[0].line, 3);
        assert_eq!(
            lints[0].message,
            "duplicate SCREENSHOT name `overview`; first used on line 2"
        );
    }

    #[test]
    fn reports_duplicate_snapshot_names_across_entries() {
        let lints = lint("VISIT /\nSNAPSHOT header\nPAGE /\nVISIT /b\nSNAPSHOT header\n");
        assert_eq!(lints.len(), 1);
        assert_eq!(lints[0].severity, Severity::Error);
        assert_eq!(lints[0].line, 5);
    }

    #[test]
    fn screenshot_and_snapshot_names_are_separate_namespaces() {
        assert_eq!(
            lint("VISIT /\nSCREENSHOT view\nSNAPSHOT view\n"),
            Vec::new()
        );
    }

    #[test]
    fn distinct_artifact_names_pass() {
        assert_eq!(lint("VISIT /\nSCREENSHOT a\nSCREENSHOT b\n"), Vec::new());
    }

    #[test]
    fn warns_about_a_presence_count_before_a_check_on_the_same_locator() {
        let lints = lint(
            "VISIT /\nASSERT testid:card count >= 1\nASSERT testid:card text contains Hello\n",
        );
        assert_eq!(lints.len(), 1);
        assert_eq!(lints[0].severity, Severity::Warning);
        assert_eq!(lints[0].line, 2);
        assert_eq!(
            lints[0].message,
            "this presence check is redundant; the check on line 3 already waits for the element"
        );
        for source in [
            "VISIT /\nASSERT css:\"li.item\" count > 0\nASSERT css:\"li.item\" count == 3\n",
            "VISIT /\nASSERT button:\"Save\" count != 0\nASSERT button:\"Save\" enabled\n",
            // `not` flips a count comparison.
            "VISIT /\nASSERT testid:card count not < 1\nASSERT testid:card visible\n",
            "VISIT /\nASSERT testid:card count >= 1\nASSERT testid:card count not == 0\n",
            // These counts reject zero, so they wait for the element too.
            "VISIT /\nASSERT testid:card count >= 1\nASSERT testid:card count < -1\n",
            "VISIT /\nASSERT testid:card count >= 1\nASSERT testid:card count == -1\n",
        ] {
            assert_eq!(lint(source).len(), 1, "source:\n{source}");
        }
    }

    #[test]
    fn presence_counts_that_are_not_redundant_pass() {
        for source in [
            // A different locator follows.
            "VISIT /\nASSERT testid:card count >= 1\nASSERT testid:other visible\n",
            // Not a presence check.
            "VISIT /\nASSERT testid:card count >= 2\nASSERT testid:card visible\n",
            // Nothing follows it.
            "VISIT /\nASSERT testid:card count >= 1\n",
            // A page check sits between them.
            "VISIT /\nASSERT testid:card count >= 1\nASSERT title == Home\nASSERT testid:card visible\n",
            // The same text through a different segment shape.
            "VISIT /\nASSERT text:Save count >= 1\nASSERT text:~Save visible\n",
            // Counts that accept zero do not wait for the element.
            "VISIT /\nASSERT testid:card count >= 1\nASSERT testid:card count not > 0\n",
            "VISIT /\nASSERT testid:card count >= 1\nASSERT testid:card count > -1\n",
            "VISIT /\nASSERT testid:card count >= 1\nASSERT testid:card count < 2\n",
        ] {
            assert_eq!(lint(source), Vec::new(), "source:\n{source}");
        }
    }

    #[test]
    fn reports_literal_expected_values_whose_type_cannot_work() {
        let lints = lint(
            "HTTP GET /x\nASSERT status == \"200\"\nASSERT bytes == \"abc\"\nASSERT bytes startsWith 12\nASSERT status > true\n",
        );
        let messages: Vec<&str> = lints.iter().map(|lint| lint.message.as_str()).collect();
        assert_eq!(messages, [
            "`==` cannot compare number values with a string literal",
            "`==` cannot compare bytes values with a string literal",
            "`startsWith` cannot compare bytes values with a number literal",
            "`>` cannot compare number values with a boolean literal",
        ]);
        let lints = lint("VISIT /\nASSERT url toDate \"%Y\" > 3\n");
        assert_eq!(lints.len(), 1);
        assert_eq!(lints[0].code, "filter-type");
        for source in [
            "HTTP GET /x\nASSERT status != \"200\"\nASSERT status not == \"200\"\nASSERT json:$.a == 1\nASSERT status == {{code}}\nASSERT bytes == hex,00;\n",
            "VISIT /\nASSERT url toDate \"%Y\" dateFormat \"%Y\" == 2026\nASSERT css:li count >= 1\n",
        ] {
            assert_eq!(lint(source), Vec::new(), "source:\n{source}");
        }
    }

    #[test]
    fn warns_when_an_http_entry_does_not_assert_status() {
        let lints = lint("HTTP GET /health\nASSERT json:$.ok == true\n");
        assert_eq!(lints.len(), 1);
        assert_eq!(lints[0].code, "unasserted-http-status");
        assert_eq!(lints[0].severity, Severity::Warning);
        assert_eq!(lints[0].line, 1);

        assert_eq!(lint("HTTP GET /health\nASSERT status == 503\n"), Vec::new());
        assert_eq!(
            lint(
                "VISIT /\nRESPONSE health GET /health\nASSERT response:health json:$.ok == true\n"
            ),
            Vec::new()
        );
    }

    #[test]
    fn setup_with_storage_is_an_error() {
        let lints = lint("[Options]\nsetup: login.whirl\nstorage: state.json\nVISIT /\n");
        assert_eq!(lints.len(), 1);
        assert_eq!(lints[0].severity, Severity::Error);
        assert_eq!(lints[0].line, 3);
        assert_eq!(
            lints[0].message,
            "storage and setup both set the starting state; use one"
        );
    }

    #[test]
    fn a_setup_reference_needs_a_setup_option() {
        let lints = lint("VISIT /u/{{setup.user_id}}\n");
        assert_eq!(lints.len(), 1);
        assert_eq!(lints[0].severity, Severity::Error);
        assert!(
            lints[0].message.contains("needs a setup option"),
            "message: {}",
            lints[0].message
        );
        assert_eq!(
            lint("[Options]\nsetup: login.whirl\nVISIT /u/{{setup.user_id}}\n"),
            Vec::new()
        );
    }

    #[test]
    fn an_interpolated_setup_path_is_an_error() {
        let lints = lint("[Options]\nsetup: {{env.LOGIN}}\nVISIT /\n");
        assert_eq!(lints.len(), 1);
        assert!(
            lints[0].message.contains("must be literal"),
            "message: {}",
            lints[0].message
        );
    }

    #[test]
    fn setup_refs_are_checked_against_the_setup_flow() {
        let setup = parse_file(
            Path::new("login.whirl"),
            "VISIT /\nCAPTURE token: css:\"#t\" text\n",
        )
        .expect("fixture should parse");
        let file = parse_file(
            Path::new("a.whirl"),
            "[Options]\nsetup: login.whirl\nVISIT /{{setup.token}}/{{setup.nope}}\n",
        )
        .expect("fixture should parse");
        let lints = lint_setup_refs(&file, &setup);
        assert_eq!(lints.len(), 1);
        assert_eq!(
            lints[0].message,
            "setup flow 'login.whirl' has no capture `nope`"
        );

        let nested = parse_file(
            Path::new("login.whirl"),
            "[Options]\nsetup: root.whirl\nVISIT /\n",
        )
        .expect("fixture should parse");
        let lints = lint_setup_refs(&file, &nested);
        assert!(
            lints
                .iter()
                .any(|lint| lint.message.contains("names its own setup")),
            "lints: {lints:?}"
        );
    }

    #[test]
    fn captures_read_by_dependents_count_as_used() {
        let file = parse_file(
            Path::new("login.whirl"),
            "VISIT /\nCAPTURE token: css:\"#t\" text\n",
        )
        .expect("fixture should parse");
        assert_eq!(lint_file(&file).len(), 1);
        let uses: HashSet<String> = ["token".to_owned()].into_iter().collect();
        assert_eq!(lint_file_with(&file, &uses), Vec::new());
    }

    #[test]
    fn warns_about_a_capture_nothing_uses() {
        let lints = lint("VISIT /\nCAPTURE order_id: testid:x text\n");
        assert_eq!(lints.len(), 1);
        assert_eq!(lints[0].severity, Severity::Warning);
        assert_eq!(lints[0].line, 2);
        assert_eq!(lints[0].message, "capture `order_id` is never used");
    }

    #[test]
    fn a_reference_in_a_later_value_is_a_use() {
        let source = "VISIT /\nCAPTURE next_url: testid:x attr:href\n\nVISIT {{next_url}}\n";
        assert_eq!(lint(source), Vec::new());
    }

    #[test]
    fn a_reference_in_a_later_locator_is_a_use() {
        let source =
            "VISIT /\nCAPTURE row: testid:x text\n\nVISIT /b\nASSERT testid:{{row}} visible\n";
        assert_eq!(lint(source), Vec::new());
    }

    #[test]
    fn an_overwrite_is_not_a_use() {
        let source = "VISIT /\nCAPTURE id: testid:x text\n\nVISIT /b\nCAPTURE id: testid:y text\n\nVISIT {{id}}\n";
        let lints = lint(source);
        assert_eq!(lints.len(), 1);
        assert_eq!(lints[0].line, 2);
        assert_eq!(lints[0].message, "capture `id` is never used");
    }

    #[test]
    fn an_overwriting_captures_own_source_is_a_use() {
        let source = "VISIT /\nCAPTURE id: testid:x text\n\nVISIT /b\nCAPTURE id: eval \"'{{id}}' + '!'\"\n\nVISIT {{id}}\n";
        assert_eq!(lint(source), Vec::new());
    }

    #[test]
    fn an_earlier_reference_is_not_a_use() {
        let source = "VISIT {{id}}\nCAPTURE id: testid:x text\n";
        let lints = lint(source);
        assert_eq!(lints.len(), 1);
        assert_eq!(lints[0].message, "capture `id` is never used");
    }

    #[test]
    fn an_env_reference_is_not_a_capture_use() {
        let source = "VISIT /\nCAPTURE id: testid:x text\n\nVISIT {{env.id}}\n";
        let lints = lint(source);
        assert_eq!(lints.len(), 1);
    }

    fn lint_act_source(source: &str) -> Vec<Lint> {
        let file = parse_file(Path::new("test.whirl"), source)
            .unwrap_or_else(|error| panic!("fixture should parse:\n{error}"));
        lint_act(&file, |model| match model {
            "anthropic/claude-sonnet-5" => ModelFacts::Known { images: Some(true) },
            "text/only" => ModelFacts::Known {
                images: Some(false),
            },
            "maybe/images" => ModelFacts::Known { images: None },
            _ => ModelFacts::Unknown,
        })
    }

    #[test]
    fn judge_needs_a_model_that_accepts_images() {
        let judge = |model: &str| {
            lint_act_source(&format!(
                "[Options]\nmodel: {model}\nVISIT /\nASSERT testid:x visible\nJUDGE \"it looks right\"\n"
            ))
        };
        assert_eq!(judge("anthropic/claude-sonnet-5"), Vec::new());
        let lints = judge("text/only");
        assert_eq!(lints.len(), 1);
        assert_eq!(lints[0].code, "judge-without-images");
        assert_eq!(lints[0].severity, Severity::Error);
        assert_eq!(lints[0].line, 2);
        let lints = judge("maybe/images");
        assert_eq!(lints.len(), 1);
        assert_eq!(lints[0].code, "judge-images-unknown");
        assert_eq!(lints[0].severity, Severity::Warning);
        // Image support matters only to JUDGE.
        assert_eq!(
            lint_act_source("[Options]\nmodel: text/only\nVISIT /\nACT \"sign in\"\n"),
            Vec::new()
        );
    }

    #[test]
    fn judge_without_a_model_is_an_error() {
        let lints = lint_act_source("VISIT /\nASSERT testid:x visible\nJUDGE \"it looks right\"\n");
        assert_eq!(lints.len(), 1);
        assert_eq!(lints[0].code, "act-without-model");
        assert_eq!(lints[0].line, 3);
        assert_eq!(
            lints[0].message,
            "JUDGE needs a `model` option naming the language model to ask"
        );
    }

    #[test]
    fn goal_is_the_last_action_of_an_entry_with_an_assert() {
        let lint_codes = |source: &str| -> Vec<(&'static str, u32)> {
            lint(source)
                .iter()
                .map(|lint| (lint.code, lint.line))
                .collect()
        };
        assert_eq!(
            lint_codes(
                "[Options]\nmodel: m\nVISIT /\nGOAL \"buy a mug\"\nASSERT testid:cart text == 1\n"
            ),
            []
        );
        assert_eq!(
            lint_codes(
                "[Options]\nmodel: m\nVISIT /\nGOAL \"buy a mug\"\nCLICK Checkout\nASSERT url exists\n"
            ),
            [("goal-unchecked", 4)]
        );
        assert_eq!(
            lint_codes(
                "[Options]\nmodel: m\nVISIT /\nGOAL \"buy a mug\"\nCAPTURE u: url\n\nVISIT {{u}}\n"
            ),
            [("goal-unchecked", 4)]
        );
        let lints = lint_act_source("VISIT /\nGOAL \"buy a mug\"\nASSERT url exists\n");
        assert_eq!(lints.len(), 1);
        assert_eq!(lints[0].code, "act-without-model");
        assert_eq!(
            lints[0].message,
            "GOAL needs a `model` option naming the language model to ask"
        );
    }

    #[test]
    fn a_judge_in_an_entry_without_an_assert_warns() {
        let lints = lint(
            "[Options]\nmodel: m\nVISIT /\nJUDGE \"a\"\nJUDGE \"b\"\n\nVISIT /b\nASSERT testid:x visible\nJUDGE \"c\"\n",
        );
        assert_eq!(lints.len(), 1);
        assert_eq!(lints[0].code, "judge-alone");
        assert_eq!(lints[0].severity, Severity::Warning);
        assert_eq!(lints[0].line, 4);
    }

    #[test]
    fn act_without_a_model_is_an_error() {
        let lints = lint_act_source("VISIT /\nACT \"add a widget to the cart\"\n");
        assert_eq!(lints.len(), 1);
        assert_eq!(lints[0].code, "act-without-model");
        assert_eq!(lints[0].severity, Severity::Error);
        assert_eq!(lints[0].line, 2);
    }

    #[test]
    fn a_known_model_passes_and_an_unknown_one_is_an_error() {
        assert_eq!(
            lint_act_source(
                "[Options]\nmodel: anthropic/claude-sonnet-5\nVISIT /\nACT \"sign in\"\n"
            ),
            Vec::new()
        );
        let lints =
            lint_act_source("[Options]\nmodel: anthropic/claude-nope\nVISIT /\nACT \"sign in\"\n");
        assert_eq!(lints.len(), 1);
        assert_eq!(lints[0].code, "unknown-model");
        assert_eq!(lints[0].line, 2);
    }

    #[test]
    fn an_interpolated_model_is_not_checked_and_no_act_needs_no_model() {
        assert_eq!(
            lint_act_source("[Options]\nmodel: {{env.MODEL}}\nVISIT /\nACT \"sign in\"\n"),
            Vec::new()
        );
        assert_eq!(lint_act_source("VISIT /\nCLICK Go\n"), Vec::new());
    }

    #[test]
    fn act_instructions_use_captures() {
        let source = "VISIT /\nCAPTURE item: testid:x text\n\nACT \"open {{item}}\"\n";
        assert_eq!(lint(source), Vec::new());
    }

    #[test]
    fn extract_names_are_unique_and_read_after_their_line() {
        let lints = lint(
            "[Options]\nmodel: m\nVISIT /\nASSERT extract:early exists\nEXTRACT early \"x\"\nEXTRACT early \"y\"\n",
        );
        let codes: Vec<&str> = lints.iter().map(|lint| lint.code).collect();
        assert_eq!(codes, ["unknown-extract", "duplicate-extract"]);
    }

    #[test]
    fn an_extract_schema_must_stay_in_the_subset() {
        let lints = lint(
            "[Options]\nmodel: m\nVISIT /\nEXTRACT n \"x\"\n{\"type\": \"string\", \"pattern\": \"a\"}\n",
        );
        assert_eq!(lints.len(), 1);
        assert_eq!(lints[0].code, "extract-schema-unsupported");
        assert_eq!(lints[0].line, 5);
    }

    #[test]
    fn an_extract_right_after_an_interaction_warns() {
        let lints = lint(
            "[Options]\nmodel: m\nVISIT /\nCLICK Go\nEXTRACT n \"x\"\nASSERT extract:n exists\n",
        );
        assert_eq!(lints.len(), 1);
        assert_eq!(lints[0].code, "extract-unsettled");
        assert_eq!(lints[0].severity, Severity::Warning);
        let settled = "[Options]\nmodel: m\nVISIT /\nCLICK Go\nASSERT url == /\nEXTRACT n \"x\"\nASSERT extract:n exists\n";
        assert_eq!(lint(settled), Vec::new());
    }

    #[test]
    fn an_ai_target_cannot_be_counted() {
        let lints = lint("[Options]\nmodel: m\nVISIT /\nASSERT ai:\"the rows\" count == 2\n");
        assert_eq!(lints.len(), 1);
        assert_eq!(lints[0].code, "ai-count");
        assert_eq!(lints[0].severity, Severity::Error);
    }

    #[test]
    fn an_ai_target_needs_a_model() {
        let lints = lint_act_source("VISIT /\nCLICK ai:\"the buy button\"\n");
        assert_eq!(lints.len(), 1);
        assert_eq!(lints[0].code, "act-without-model");
        assert_eq!(
            lints[0].message,
            "`ai:` needs a `model` option naming the language model to ask"
        );
        assert_eq!(lints[0].column, 7);
    }

    #[test]
    fn keyword_checks_keep_the_check_rules() {
        let lints = lint(
            "HTTP GET /api\nASSERT json:$.id exists\nVISIT /\nASSERT testid:card count >= 1\nASSERT testid:card visible\nCAPTURE unused: url\n",
        );
        let codes: Vec<&str> = lints.iter().map(|lint| lint.code).collect();
        assert_eq!(codes, [
            "unasserted-http-status",
            "redundant-presence",
            "unused-capture"
        ]);
    }

    #[test]
    fn a_capture_between_two_checks_breaks_the_presence_pair() {
        let source = "VISIT /\nASSERT testid:card count >= 1\nCAPTURE n: url\nASSERT testid:card visible\nVISIT {{n}}\n";
        assert_eq!(lint(source), Vec::new());
    }

    #[test]
    fn lints_come_back_in_line_order() {
        let source =
            "VISIT /\nCAPTURE unused: testid:x text\n\nVISIT /b\nSCREENSHOT a\nSCREENSHOT a\n";
        let lints = lint(source);
        assert_eq!(lints.len(), 2);
        assert_eq!(lints[0].line, 2);
        assert_eq!(lints[1].line, 6);
        assert_eq!(lints[1].severity, Severity::Error);
    }
}
