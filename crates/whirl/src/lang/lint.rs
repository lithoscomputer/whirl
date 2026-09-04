//! Lint rules for parsed `.whirl` files (SPEC sections 14, 16).
//!
//! [`lint_file`] reports diagnostics for one file. Errors stop the
//! invocation with exit code 2 like parse errors; warnings do not change
//! the exit code (SPEC 16). The CLI layer owns both mappings.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

use crate::lang::ast::{
    Action, ActionKind, Assert, AssertBody, Capture, CaptureSource, Entry, File, FileOption,
    Locator, NumOp, PageCheck, ResponseField, SegmentKind, Span, StateCheck, StrCheck, Value,
    ValueSegment,
};

/// How serious a lint diagnostic is: an [`Severity::Error`] fails
/// `whirl check` and `whirl` runs with exit code 2; a
/// [`Severity::Warning`] is reported without changing the exit code.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Severity {
    Error,
    Warning,
}

/// One lint diagnostic, located like a parse error (SPEC 16).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Lint {
    pub code:     &'static str,
    pub severity: Severity,
    pub path:     PathBuf,
    /// 1-based line of the offending token.
    pub line:     u32,
    /// 1-based character column of the offending token.
    pub column:   u32,
    /// Length of the offending token in characters.
    pub len:      u32,
    pub message:  String,
}

/// Lints one parsed file: duplicate `SCREENSHOT` or `SNAPSHOT` names are
/// errors (SPEC 14), and a capture whose value no later line reads is a
/// warning (SPEC 16). Diagnostics come back in line order.
pub fn lint_file(file: &File) -> Vec<Lint> {
    lint_file_with(file, &HashSet::new())
}

/// [`lint_file`] for a file other files depend on through `setup:`:
/// `external_uses` names the captures those files read as
/// `{{setup.name}}`, which count as used here (SPEC 16).
pub fn lint_file_with(file: &File, external_uses: &HashSet<String>) -> Vec<Lint> {
    let mut lints = Vec::new();
    duplicate_artifact_names(file, &mut lints);
    tab_names(file, &mut lints);
    response_names(file, &mut lints);
    unused_captures(file, external_uses, &mut lints);
    setup_option_rules(file, &mut lints);
    redundant_presence_counts(file, &mut lints);
    lints.sort_by_key(|lint| (lint.line, lint.column));
    lints
}

/// The capture names a file reads as `{{setup.name}}`.
pub fn setup_capture_uses(file: &File) -> HashSet<String> {
    collect_setup_refs(file)
        .into_iter()
        .map(|var_ref| var_ref.name.to_owned())
        .collect()
}

/// Lints a file against its parsed `setup` flow (SPEC 12, 16): the setup
/// flow may not name a setup of its own, and every `{{setup.name}}` the
/// file reads must be a capture the setup flow takes.
pub fn lint_setup_refs(file: &File, setup: &File) -> Vec<Lint> {
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

/// Warns about a `count >= 1` (or `count > 0`, `count != 0`) assert
/// directly followed by a check on the same locator that requires
/// presence (SPEC 16). Checks accepting zero matches preserve the wait.
fn redundant_presence_counts(file: &File, lints: &mut Vec<Lint>) {
    for entry in &file.entries {
        for pair in entry.asserts.windows(2) {
            let AssertBody::ElementCount { locator, op, count } = &pair[0].body else {
                continue;
            };
            let asserts_presence =
                matches!((op, count), (NumOp::Ge, 1) | (NumOp::Gt | NumOp::Ne, 0));
            if !asserts_presence {
                continue;
            }
            let next = match &pair[1].body {
                AssertBody::ElementState {
                    state: StateCheck::Hidden,
                    ..
                }
                | AssertBody::Url(_)
                | AssertBody::Title(_)
                | AssertBody::TabClosed { .. }
                | AssertBody::ResponseStatus { .. }
                | AssertBody::ResponseValue { .. } => continue,
                AssertBody::ElementState { locator, .. }
                | AssertBody::ElementValue { locator, .. } => locator,
                AssertBody::ElementCount { locator, op, count } => {
                    let accepts_zero = match op {
                        NumOp::Eq | NumOp::Ge => *count == 0,
                        NumOp::Ne | NumOp::Lt => *count != 0,
                        NumOp::Le => true,
                        NumOp::Gt => false,
                    };
                    if accepts_zero {
                        continue;
                    }
                    locator
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
        if let FileOption::Setup(value) = &line.option {
            if !value.is_literal() {
                lints.push(lint_at(
                    file,
                    Severity::Error,
                    "interpolated-setup",
                    value.span,
                    "the setup path must be literal; it is resolved before any variable exists"
                        .to_owned(),
                ));
            }
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
    if let Some(page) = &entry.page {
        if let PageCheck::Value(value) = &page.check {
            collect_value_refs(value, page.line, refs);
        }
    }
    for assert in &entry.asserts {
        collect_assert_refs(assert, refs);
    }
    for capture in &entry.captures {
        match &capture.source {
            CaptureSource::Response { field, .. } => {
                collect_response_field_refs(field, capture.line, refs);
            }
            CaptureSource::Element { locator, .. } => {
                collect_locator_refs(locator, capture.line, refs);
            }
            CaptureSource::Eval(script) => collect_value_refs(script, capture.line, refs),
            CaptureSource::Url | CaptureSource::Title => {}
        }
    }
}

fn collect_action_refs<'a>(action: &'a Action, refs: &mut Vec<VarRef<'a>>) {
    let line = action.line;
    match &action.kind {
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
        AssertBody::ResponseValue { field, check, .. } => {
            collect_response_field_refs(field, line, refs);
            collect_check_refs(check, line, refs);
        }
        AssertBody::ResponseStatus { .. } | AssertBody::TabClosed { .. } => {}
        AssertBody::ElementState { locator, .. } | AssertBody::ElementCount { locator, .. } => {
            collect_locator_refs(locator, line, refs);
        }
        AssertBody::ElementValue { locator, check, .. } => {
            collect_locator_refs(locator, line, refs);
            collect_check_refs(check, line, refs);
        }
        AssertBody::Url(check) | AssertBody::Title(check) => collect_check_refs(check, line, refs),
    }
}

fn collect_check_refs<'a>(check: &'a StrCheck, line: u32, refs: &mut Vec<VarRef<'a>>) {
    match check {
        StrCheck::Eq(value) | StrCheck::Ne(value) | StrCheck::Contains(value) => {
            collect_value_refs(value, line, refs);
        }
        StrCheck::Matches(_) => {}
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
            | FileOption::Setup(value) => values.push((value, line.line)),
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
        ResponseField::Status => {}
        ResponseField::Header(value) | ResponseField::Json(value) => {
            collect_value_refs(value, line, refs);
        }
    }
}

fn response_names(file: &File, lints: &mut Vec<Lint>) {
    let mut names = HashSet::new();
    for entry in &file.entries {
        for action in &entry.actions {
            if let ActionKind::Response { name, .. } = &action.kind {
                if !names.insert(name.text.as_str()) {
                    lints.push(lint_at(
                        file,
                        Severity::Error,
                        "duplicate-response",
                        name.span,
                        format!("response `{}` is already named", name.text),
                    ));
                }
            }
        }
        let assertion_names = entry
            .asserts
            .iter()
            .filter_map(|assertion| match &assertion.body {
                AssertBody::ResponseStatus { name, .. }
                | AssertBody::ResponseValue { name, .. } => Some(name),
                _ => None,
            });
        let capture_names = entry
            .captures
            .iter()
            .filter_map(|capture| match &capture.source {
                CaptureSource::Response { name, .. } => Some(name),
                _ => None,
            });
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
            if let AssertBody::TabClosed { name } = &assertion.body {
                if !names.contains(name.text.as_str()) {
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
}

#[cfg(test)]
mod tests {
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
        ] {
            assert_eq!(lint(source), Vec::new(), "source:\n{source}");
        }
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
