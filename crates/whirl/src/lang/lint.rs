//! Lint rules for parsed `.whirl` files (SPEC sections 14, 16).
//!
//! [`lint_file`] reports diagnostics for one file. Errors stop the
//! invocation with exit code 2 like parse errors; warnings do not change
//! the exit code (SPEC 16). The CLI layer owns both mappings.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

use crate::check::{Number, PredicateKind, StaticType, ValueType, is_bytes_literal_shape};
use crate::lang::ast::{
    Action, ActionKind, Assert, AssertBody, Capture, CheckLine, Entry, Extractor, File, FileOption,
    FilterArg, FilterSpec, Ident, Locator, Operand, PageCheck, PredicateSpec, ResponseField,
    SegmentKind, Span, StateCheck, Subject, Value, ValueSegment, chain_type,
};

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
    tab_names(file, &mut lints);
    response_names(file, &mut lints);
    unasserted_http_status(file, &mut lints);
    unused_captures(file, external_uses, &mut lints);
    setup_option_rules(file, &mut lints);
    redundant_presence_counts(file, &mut lints);
    filter_types(file, &mut lints);
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
        .flat_map(|entry| &entry.captures)
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

/// `ACT` rules (SPEC 5, 7.4): a file that uses `ACT` needs a `model`
/// option, and a literal model must be one `known_model` accepts. The
/// caller decides what is known, because it depends on the environment
/// (SPEC 13).
pub(crate) fn lint_act(file: &File, known_model: impl Fn(&str) -> bool) -> Vec<Lint> {
    if let Some(line) = file.model_option() {
        let FileOption::Model(value) = &line.option else {
            return Vec::new();
        };
        return match value.as_literal() {
            Some(model) if !known_model(&model) => vec![lint_at(
                file,
                Severity::Error,
                "unknown-model",
                value.span,
                format!(
                    "unknown model `{model}`; use a provider/model name such as anthropic/claude-sonnet-5"
                ),
            )],
            _ => Vec::new(),
        };
    }
    file.entries
        .iter()
        .flat_map(|entry| &entry.actions)
        .find(|action| matches!(action.kind, ActionKind::Act { .. }))
        .map(|action| {
            lint_at(
                file,
                Severity::Error,
                "act-without-model",
                action.span,
                "ACT needs a `model` option naming the language model to ask".to_owned(),
            )
        })
        .into_iter()
        .collect()
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
        for pair in entry.asserts.windows(2) {
            let Some((locator, kind, count)) = count_check(&pair[0]) else {
                continue;
            };
            let asserts_presence = matches!(
                (kind, count),
                (PredicateKind::Ge, 1) | (PredicateKind::Gt | PredicateKind::Ne, 0)
            );
            if !asserts_presence {
                continue;
            }
            let next = if let Some((locator, kind, count)) = count_check(&pair[1]) {
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
        for assert in &entry.asserts {
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
        for capture in &entry.captures {
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
                ActionKind::Snapshot { name } => Some(("SNAPSHOT", name)),
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
    let captures: Vec<&Capture> = file
        .entries
        .iter()
        .flat_map(|entry| &entry.captures)
        .collect();
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
    for assert in &entry.asserts {
        collect_assert_refs(assert, refs);
    }
    for capture in &entry.captures {
        collect_chain_refs(&capture.subject, &capture.filters, capture.line, refs);
    }
}

fn collect_action_refs<'a>(action: &'a Action, refs: &mut Vec<VarRef<'a>>) {
    let line = action.line;
    match &action.kind {
        ActionKind::Http {
            url, headers, body, ..
        } => {
            collect_value_refs(url, line, refs);
            for header in headers {
                collect_value_refs(&header.value, header.line, refs);
            }
            if let Some(body) = body {
                collect_value_refs(&body.value, body.line, refs);
            }
        }
        ActionKind::Response { url, .. } | ActionKind::Visit { url } => {
            collect_value_refs(url, line, refs);
        }
        ActionKind::Click { target }
        | ActionKind::Dblclick { target }
        | ActionKind::Check { target }
        | ActionKind::Uncheck { target }
        | ActionKind::Hover { target } => collect_locator_refs(target, line, refs),
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
        ActionKind::Upload { target, path } => {
            collect_locator_refs(target, line, refs);
            collect_value_refs(path, line, refs);
        }
        ActionKind::Eval { script } => collect_value_refs(script, line, refs),
        ActionKind::Act { instruction } => collect_value_refs(instruction, line, refs),
        ActionKind::Store { key, value, .. } => {
            collect_value_refs(key, line, refs);
            collect_value_refs(value, line, refs);
        }
        ActionKind::Popup { .. }
        | ActionKind::Tab { .. }
        | ActionKind::Close { .. }
        | ActionKind::Screenshot { .. }
        | ActionKind::Snapshot { .. } => {}
    }
}

fn collect_assert_refs<'a>(assert: &'a Assert, refs: &mut Vec<VarRef<'a>>) {
    let line = assert.line;
    match &assert.body {
        AssertBody::TabClosed { .. } => {}
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
        Subject::Url | Subject::Title => {}
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
            | SegmentKind::Default(value) => collect_value_refs(value, line, refs),
            SegmentKind::Nth(_) => {}
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
    let mut values: Vec<(&Value, u32)> = Vec::new();
    for line in &file.options {
        match &line.option {
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
    let mut refs = Vec::new();
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
            .asserts
            .iter()
            .filter_map(|assertion| match &assertion.body {
                AssertBody::Check(check) => response_name_of(&check.subject),
                _ => None,
            });
        let capture_names = entry
            .captures
            .iter()
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

/// The `RESPONSE` name a subject reads, if any.
fn response_name_of(subject: &Subject) -> Option<&Ident> {
    match subject {
        Subject::Response {
            name: Some(name), ..
        } => Some(name),
        _ => None,
    }
}

fn unasserted_http_status(file: &File, lints: &mut Vec<Lint>) {
    for entry in &file.entries {
        let Some(action) = entry.actions.first() else {
            continue;
        };
        if !matches!(action.kind, ActionKind::Http { .. })
            || entry.asserts.iter().any(|assert| {
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

fn tab_names(file: &File, lints: &mut Vec<Lint>) {
    let mut names = HashSet::from(["main"]);
    for entry in &file.entries {
        for action in &entry.actions {
            match &action.kind {
                ActionKind::Popup { name } => {
                    if !names.insert(&name.text) {
                        lints.push(lint_at(
                            file,
                            Severity::Error,
                            "duplicate-tab",
                            name.span,
                            format!("tab `{}` is already named", name.text),
                        ));
                    }
                }
                ActionKind::Tab { name } | ActionKind::Close { name }
                    if !names.contains(name.text.as_str()) =>
                {
                    lints.push(lint_at(
                        file,
                        Severity::Error,
                        "unknown-tab",
                        name.span,
                        format!("unknown tab `{}`; name it with POPUP first", name.text),
                    ));
                }
                _ => {}
            }
        }
        for assertion in &entry.asserts {
            if let AssertBody::TabClosed { name } = &assertion.body
                && !names.contains(name.text.as_str())
            {
                lints.push(lint_at(
                    file,
                    Severity::Error,
                    "unknown-tab",
                    name.span,
                    format!("unknown tab `{}`", name.text),
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
        let lints =
            lint("VISIT /\n[Asserts]\ntestid:card count >= 1\ntestid:card text contains Hello\n");
        assert_eq!(lints.len(), 1);
        assert_eq!(lints[0].severity, Severity::Warning);
        assert_eq!(lints[0].line, 3);
        assert_eq!(
            lints[0].message,
            "this presence check is redundant; the check on line 4 already waits for the element"
        );
        for source in [
            "VISIT /\n[Asserts]\ncss:\"li.item\" count > 0\ncss:\"li.item\" count == 3\n",
            "VISIT /\n[Asserts]\nrole:button \"Save\" count != 0\nrole:button \"Save\" enabled\n",
            // `not` flips a count comparison.
            "VISIT /\n[Asserts]\ntestid:card count not < 1\ntestid:card visible\n",
            "VISIT /\n[Asserts]\ntestid:card count >= 1\ntestid:card count not == 0\n",
            // These counts reject zero, so they wait for the element too.
            "VISIT /\n[Asserts]\ntestid:card count >= 1\ntestid:card count < -1\n",
            "VISIT /\n[Asserts]\ntestid:card count >= 1\ntestid:card count == -1\n",
        ] {
            assert_eq!(lint(source).len(), 1, "source:\n{source}");
        }
    }

    #[test]
    fn presence_counts_that_are_not_redundant_pass() {
        for source in [
            // A different locator follows.
            "VISIT /\n[Asserts]\ntestid:card count >= 1\ntestid:other visible\n",
            // Not a presence check.
            "VISIT /\n[Asserts]\ntestid:card count >= 2\ntestid:card visible\n",
            // Nothing follows it.
            "VISIT /\n[Asserts]\ntestid:card count >= 1\n",
            // A page check sits between them.
            "VISIT /\n[Asserts]\ntestid:card count >= 1\ntitle == Home\ntestid:card visible\n",
            // The same text through a different segment shape.
            "VISIT /\n[Asserts]\ntext:Save count >= 1\ntext~:Save visible\n",
            // Counts that accept zero do not wait for the element.
            "VISIT /\n[Asserts]\ntestid:card count >= 1\ntestid:card count not > 0\n",
            "VISIT /\n[Asserts]\ntestid:card count >= 1\ntestid:card count > -1\n",
            "VISIT /\n[Asserts]\ntestid:card count >= 1\ntestid:card count < 2\n",
        ] {
            assert_eq!(lint(source), Vec::new(), "source:\n{source}");
        }
    }

    #[test]
    fn reports_literal_expected_values_whose_type_cannot_work() {
        let lints = lint(
            "HTTP GET /x\n[Asserts]\nstatus == \"200\"\nbytes == \"abc\"\nbytes startsWith 12\nstatus > true\n",
        );
        let messages: Vec<&str> = lints.iter().map(|lint| lint.message.as_str()).collect();
        assert_eq!(messages, [
            "`==` cannot compare number values with a string literal",
            "`==` cannot compare bytes values with a string literal",
            "`startsWith` cannot compare bytes values with a number literal",
            "`>` cannot compare number values with a boolean literal",
        ]);
        let lints = lint("VISIT /\n[Asserts]\nurl toDate \"%Y\" > 3\n");
        assert_eq!(lints.len(), 1);
        assert_eq!(lints[0].code, "filter-type");
        for source in [
            "HTTP GET /x\n[Asserts]\nstatus != \"200\"\nstatus not == \"200\"\njson:$.a == 1\nstatus == {{code}}\nbytes == hex,00;\n",
            "VISIT /\n[Asserts]\nurl toDate \"%Y\" dateFormat \"%Y\" == 2026\ncss:li count >= 1\n",
        ] {
            assert_eq!(lint(source), Vec::new(), "source:\n{source}");
        }
    }

    #[test]
    fn warns_when_an_http_entry_does_not_assert_status() {
        let lints = lint("HTTP GET /health\n[Asserts]\njson:$.ok == true\n");
        assert_eq!(lints.len(), 1);
        assert_eq!(lints[0].code, "unasserted-http-status");
        assert_eq!(lints[0].severity, Severity::Warning);
        assert_eq!(lints[0].line, 1);

        assert_eq!(
            lint("HTTP GET /health\n[Asserts]\nstatus == 503\n"),
            Vec::new()
        );
        assert_eq!(
            lint(
                "VISIT /\nRESPONSE health GET /health\n[Asserts]\nresponse:health json:$.ok == true\n"
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
            "VISIT /\n[Captures]\ntoken: css:\"#t\" text\n",
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
            "VISIT /\n[Captures]\ntoken: css:\"#t\" text\n",
        )
        .expect("fixture should parse");
        assert_eq!(lint_file(&file).len(), 1);
        let uses: HashSet<String> = ["token".to_owned()].into_iter().collect();
        assert_eq!(lint_file_with(&file, &uses), Vec::new());
    }

    #[test]
    fn warns_about_a_capture_nothing_uses() {
        let lints = lint("VISIT /\n[Captures]\norder_id: testid:x text\n");
        assert_eq!(lints.len(), 1);
        assert_eq!(lints[0].severity, Severity::Warning);
        assert_eq!(lints[0].line, 3);
        assert_eq!(lints[0].message, "capture `order_id` is never used");
    }

    #[test]
    fn a_reference_in_a_later_value_is_a_use() {
        let source = "VISIT /\n[Captures]\nnext_url: testid:x attr:href\n\nVISIT {{next_url}}\n";
        assert_eq!(lint(source), Vec::new());
    }

    #[test]
    fn a_reference_in_a_later_locator_is_a_use() {
        let source = "VISIT /\n[Captures]\nrow: testid:x text\n\nVISIT /b\n[Asserts]\ntestid:{{row}} visible\n";
        assert_eq!(lint(source), Vec::new());
    }

    #[test]
    fn an_overwrite_is_not_a_use() {
        let source = "VISIT /\n[Captures]\nid: testid:x text\n\nVISIT /b\n[Captures]\nid: testid:y text\n\nVISIT {{id}}\n";
        let lints = lint(source);
        assert_eq!(lints.len(), 1);
        assert_eq!(lints[0].line, 3);
        assert_eq!(lints[0].message, "capture `id` is never used");
    }

    #[test]
    fn an_overwriting_captures_own_source_is_a_use() {
        let source = "VISIT /\n[Captures]\nid: testid:x text\n\nVISIT /b\n[Captures]\nid: eval \"'{{id}}' + '!'\"\n\nVISIT {{id}}\n";
        assert_eq!(lint(source), Vec::new());
    }

    #[test]
    fn an_earlier_reference_is_not_a_use() {
        let source = "VISIT {{id}}\n[Captures]\nid: testid:x text\n";
        let lints = lint(source);
        assert_eq!(lints.len(), 1);
        assert_eq!(lints[0].message, "capture `id` is never used");
    }

    #[test]
    fn an_env_reference_is_not_a_capture_use() {
        let source = "VISIT /\n[Captures]\nid: testid:x text\n\nVISIT {{env.id}}\n";
        let lints = lint(source);
        assert_eq!(lints.len(), 1);
    }

    fn lint_act_source(source: &str) -> Vec<Lint> {
        let file = parse_file(Path::new("test.whirl"), source)
            .unwrap_or_else(|error| panic!("fixture should parse:\n{error}"));
        lint_act(&file, |model| model == "anthropic/claude-sonnet-5")
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
        let source = "VISIT /\n[Captures]\nitem: testid:x text\n\nACT \"open {{item}}\"\n";
        assert_eq!(lint(source), Vec::new());
    }

    #[test]
    fn lints_come_back_in_line_order() {
        let source =
            "VISIT /\n[Captures]\nunused: testid:x text\n\nVISIT /b\nSCREENSHOT a\nSCREENSHOT a\n";
        let lints = lint(source);
        assert_eq!(lints.len(), 2);
        assert_eq!(lints[0].line, 3);
        assert_eq!(lints[1].line, 7);
        assert_eq!(lints[1].severity, Severity::Error);
    }
}
