//! Lint rules for parsed `.whirl` files (SPEC sections 14, 16).
//!
//! [`lint_file`] reports diagnostics for one file. Errors stop the
//! invocation with exit code 2 like parse errors; warnings do not change
//! the exit code (SPEC 16). The CLI layer owns both mappings.

use std::collections::HashMap;
use std::path::PathBuf;

use crate::lang::ast::{
    Action, ActionKind, Assert, AssertBody, Capture, CaptureSource, Entry, File, Locator,
    PageCheck, SegmentKind, Span, StrCheck, Value, ValueSegment,
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
    let mut lints = Vec::new();
    duplicate_artifact_names(file, &mut lints);
    unused_captures(file, &mut lints);
    lints.sort_by_key(|lint| (lint.line, lint.column));
    lints
}

fn lint_at(file: &File, severity: Severity, span: Span, message: String) -> Lint {
    Lint {
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
                lints.push(lint_at(file, Severity::Error, name.span, message));
            }
            None => {
                first_lines.insert((kind, name.text.as_str()), name.span.line);
            }
        }
    }
}

/// A `{{name}}` variable reference and the line it appears on.
struct VarRef<'a> {
    name: &'a str,
    line: u32,
}

/// Warns about captures no later line uses (SPEC 16). A use is a
/// `{{name}}` reference in a value on a later line, up to and including
/// the line of the next capture that overwrites the name: that capture's
/// own source still reads the old value, but overwriting is not a use.
fn unused_captures(file: &File, lints: &mut Vec<Lint>) {
    let refs = collect_var_refs(file);
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
        let used = refs.iter().any(|var_ref| {
            var_ref.name == capture.name.text
                && var_ref.line > capture.line
                && overwrite_line.is_none_or(|line| var_ref.line <= line)
        });
        if !used {
            let message = format!("capture `{}` is never used", capture.name.text);
            lints.push(lint_at(file, Severity::Warning, capture.name.span, message));
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
        ActionKind::Visit { url } => collect_value_refs(url, line, refs),
        ActionKind::Click { target }
        | ActionKind::Dblclick { target }
        | ActionKind::Check { target }
        | ActionKind::Uncheck { target }
        | ActionKind::Hover { target } => collect_locator_refs(target, line, refs),
        ActionKind::Fill { target, value }
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
        ActionKind::Screenshot { .. } | ActionKind::Snapshot { .. } => {}
    }
}

fn collect_assert_refs<'a>(assert: &'a Assert, refs: &mut Vec<VarRef<'a>>) {
    let line = assert.line;
    match &assert.body {
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
            | SegmentKind::Default(value) => collect_value_refs(value, line, refs),
            SegmentKind::Nth(_) => {}
        }
    }
}

fn collect_value_refs<'a>(value: &'a Value, line: u32, refs: &mut Vec<VarRef<'a>>) {
    for segment in &value.segments {
        if let ValueSegment::Var(name) = segment {
            refs.push(VarRef { name, line });
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
