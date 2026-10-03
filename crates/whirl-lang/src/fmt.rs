//! Canonical formatter for parsed `.whirl` files (SPEC sections 3.1, 13).
//!
//! [`format_file`] renders a parsed [`File`] in canonical form: single
//! spaces between tokens, quotes only where a value requires them, no
//! blank lines inside an entry, and one blank line between entries and
//! after the `[Options]` section. Comments stay in place: a full-line
//! comment keeps its own line, and a trailing comment stays on its step's
//! line with one space before `#`.
//!
//! The formatter never removes quotes whose removal would change the
//! parse (SPEC 3.1): a value that would start with `@` or `#`, a keyword,
//! a colon or `>>` in unprefixed locator text, or a lone `*` role name
//! stays quoted, as does any value with whitespace, `"`, or characters that
//! need escapes. Formatting is idempotent, and re-parsing the output yields
//! a structurally identical file.

use std::fmt::Write as _;

use whirl_types::{Number, is_bytes_literal_shape};

use crate::ast::snapshot::SnapshotOption;
use crate::ast::{
    Action, ActionKind, Assert, AssertBody, Capture, CheckLine, CheckStep, Comment, DurationLit,
    DurationUnit, Entry, Extractor, File, FileOption, FilterArg, FilterSpec, HttpBodyKind, Judge,
    Locator, MockResponse, Operand, OptionValue, Page, PageCheck, PredicateSpec, Regex,
    RequestField, ResponseField, ScrollDirection, ScrollMotion, SegmentKind, StateCheck, Subject,
    TextPrefix, Value, ValueSegment, Viewport,
};

/// Where a rendered value sits in its line. The context decides which
/// bare spellings would change the parse and therefore need quotes.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ValueCtx {
    /// A standalone value: URLs, fill text, keys, scripts, filter
    /// arguments, and option and header values.
    Plain,
    /// The `PAGE` value; a bare `matches` would start a regex check.
    Page,
    /// Unprefixed locator text in an action; a colon would make it a
    /// prefix, and `>>` would be a separator.
    Unprefixed,
    /// The first segment right after `SCROLL`, where a bare direction or
    /// `to` is the motion.
    ScrollFirst,
    /// A role's accessible name, matched exactly: a bare `*` means any
    /// name, and a leading `~` would mark a substring match.
    RoleName,
    /// A text prefix's value, matched exactly; a leading `~` would mark a
    /// substring match.
    Exact,
    /// A role's accessible name after `:~`, where `*` would be an error.
    RoleSubstring,
    /// A value attached to a prefix such as `css:` or `file:`; the prefix
    /// shields it from keyword and timeout readings.
    Prefixed,
    /// A check's expected value; a bare `[` would start a JSON literal.
    Operand,
}

/// True when a character can sit in a bare token without changing the
/// parse. Whitespace and `"` end a bare token; `\` and `{` take part in
/// escapes and interpolation; control characters need `\u{...}` escapes,
/// which only quoted values have.
fn bare_safe_char(ch: char) -> bool {
    !ch.is_whitespace() && !ch.is_control() && !matches!(ch, '"' | '\\' | '{')
}

/// The bare spelling of a value, or `None` when no bare spelling parses
/// back to the same value.
fn bare_candidate(value: &Value) -> Option<String> {
    let mut text = String::new();
    for segment in &value.segments {
        match segment {
            ValueSegment::Literal(literal) => {
                if !literal.chars().all(bare_safe_char) {
                    return None;
                }
                text.push_str(literal);
            }
            ValueSegment::Var(name) => {
                let _ = write!(text, "{{{{{name}}}}}");
            }
            ValueSegment::EnvVar(name) => {
                let _ = write!(text, "{{{{env.{name}}}}}");
            }
            ValueSegment::SetupVar(name) => {
                let _ = write!(text, "{{{{setup.{name}}}}}");
            }
        }
    }
    if text.is_empty() { None } else { Some(text) }
}

/// True when the bare spelling would parse as something other than this
/// value in its context (SPEC 3.1: `whirl fmt` never removes quotes
/// whose removal would change the parse).
fn bare_changes_parse(text: &str, ctx: ValueCtx) -> bool {
    let unprefixed = || text.contains(':') || text == ">>";
    match ctx {
        ValueCtx::Prefixed => false,
        ValueCtx::RoleName => text == "*" || text.starts_with('~'),
        ValueCtx::Exact => text.starts_with('~'),
        ValueCtx::RoleSubstring => text == "*",
        // A bare token that starts with `@` is a timeout, and one that
        // starts with `#` is a comment (SPEC 3).
        _ if text.starts_with(['@', '#']) => true,
        ValueCtx::Plain => false,
        ValueCtx::Operand => text.starts_with('['),
        ValueCtx::Page => text == "matches",
        ValueCtx::Unprefixed => unprefixed(),
        ValueCtx::ScrollFirst => {
            unprefixed() || text == "to" || ScrollDirection::from_keyword(text).is_some()
        }
    }
}

/// Renders a value in quotes, escaping per SPEC 3.1: `\"`, `\\`, `\n`,
/// `\t`, `\u{XXXX}` for other control characters, and `\{` for every
/// literal `{` so no `{{` sequence forms by accident.
fn render_quoted(value: &Value) -> String {
    let mut out = String::from("\"");
    for segment in &value.segments {
        match segment {
            ValueSegment::Literal(literal) => {
                for ch in literal.chars() {
                    match ch {
                        '"' => out.push_str("\\\""),
                        '\\' => out.push_str("\\\\"),
                        '\n' => out.push_str("\\n"),
                        '\t' => out.push_str("\\t"),
                        '{' => out.push_str("\\{"),
                        ch if ch.is_control() => {
                            let _ = write!(out, "\\u{{{:X}}}", u32::from(ch));
                        }
                        ch => out.push(ch),
                    }
                }
            }
            ValueSegment::Var(name) => {
                let _ = write!(out, "{{{{{name}}}}}");
            }
            ValueSegment::EnvVar(name) => {
                let _ = write!(out, "{{{{env.{name}}}}}");
            }
            ValueSegment::SetupVar(name) => {
                let _ = write!(out, "{{{{setup.{name}}}}}");
            }
        }
    }
    out.push('"');
    out
}

/// Renders a value: bare when a bare spelling parses identically in this
/// context, quoted otherwise.
fn render_value(value: &Value, ctx: ValueCtx) -> String {
    match bare_candidate(value) {
        Some(text) if !bare_changes_parse(&text, ctx) => text,
        _ => render_quoted(value),
    }
}

fn render_duration(duration: DurationLit) -> String {
    match duration.unit {
        DurationUnit::Milliseconds => format!("{}ms", duration.amount),
        DurationUnit::Seconds => format!("{}s", duration.amount),
    }
}

fn render_regex(regex: &Regex) -> String {
    let mut out = format!("/{}/", regex.pattern);
    if regex.flags.ignore_case {
        out.push('i');
    }
    if regex.flags.dot_all {
        out.push('s');
    }
    if regex.flags.multiline {
        out.push('m');
    }
    out
}

/// Renders one locator segment as its token (SPEC 6.1). `first_ctx` is
/// the context of unprefixed text in the locator's first segment.
fn render_segment(kind: &SegmentKind, unprefixed_ctx: ValueCtx) -> String {
    let prefixed = |value: &Value| render_value(value, ValueCtx::Prefixed);
    match kind {
        SegmentKind::Role {
            substring,
            role,
            name,
        } => match name {
            Some(name) if *substring => {
                format!("{role}:~{}", render_value(name, ValueCtx::RoleSubstring))
            }
            Some(name) => format!("{role}:{}", render_value(name, ValueCtx::RoleName)),
            None => format!("{role}:*"),
        },
        SegmentKind::TextEngine {
            prefix,
            substring,
            value,
        } => {
            let name = match prefix {
                TextPrefix::Label => "label",
                TextPrefix::Placeholder => "placeholder",
                TextPrefix::Text => "text",
                TextPrefix::Alt => "alt",
                TextPrefix::Title => "title",
            };
            if *substring {
                format!("{name}:~{}", prefixed(value))
            } else {
                format!("{name}:{}", render_value(value, ValueCtx::Exact))
            }
        }
        SegmentKind::TestId(value) => format!("testid:{}", prefixed(value)),
        SegmentKind::Css(value) => format!("css:{}", prefixed(value)),
        SegmentKind::Frame(value) => format!("frame:{}", prefixed(value)),
        SegmentKind::Nth(index) => format!("nth:{index}"),
        SegmentKind::Default(value) => render_value(value, unprefixed_ctx),
        SegmentKind::Ai(value) => format!("ai:{}", prefixed(value)),
        SegmentKind::Ref(element) => format!("ref:{element}"),
    }
}

/// Renders a locator: segments joined by ` >> `.
fn render_locator(locator: &Locator) -> String {
    render_locator_from(locator, ValueCtx::Unprefixed)
}

/// Renders a locator whose first segment's unprefixed text sits in
/// `first_ctx`.
fn render_locator_from(locator: &Locator, first_ctx: ValueCtx) -> String {
    locator
        .segments
        .iter()
        .enumerate()
        .map(|(index, segment)| {
            let ctx = if index == 0 {
                first_ctx
            } else {
                ValueCtx::Unprefixed
            };
            render_segment(&segment.kind, ctx)
        })
        .collect::<Vec<_>>()
        .join(" >> ")
}

/// Appends an optional ` @duration` timeout suffix.
fn push_timeout(out: &mut String, timeout: Option<DurationLit>) {
    if let Some(duration) = timeout {
        let _ = write!(out, " @{}", render_duration(duration));
    }
}

/// Renders an action line (SPEC 7).
pub fn render_action(action: &Action) -> String {
    let plain = |value: &Value| render_value(value, ValueCtx::Plain);
    let mut out = match &action.kind {
        ActionKind::Http { method, url, .. } => format!("HTTP {method} {}", plain(url)),
        ActionKind::Mock {
            method,
            url,
            response,
            ..
        } => {
            let answer = match response {
                MockResponse::Fulfill { status, .. } => status.to_string(),
                MockResponse::Failed => "failed".to_owned(),
            };
            format!("MOCK {method} {} {answer}", plain(url))
        }
        ActionKind::Response { name, method, url } => {
            format!("RESPONSE {} {method} {}", name.text, plain(url))
        }
        ActionKind::Popup { name } => format!("POPUP {}", name.text),
        ActionKind::Window { name } => format!("WINDOW {}", name.text),
        ActionKind::Close { name } => format!("CLOSE {}", name.text),
        ActionKind::Visit { url } => format!("VISIT {}", plain(url)),
        ActionKind::Click { target, button } => {
            format!("{} {}", button.keyword(), render_locator(target))
        }
        ActionKind::Dblclick { target } => format!("DBLCLICK {}", render_locator(target)),
        ActionKind::Fill { target, value } => {
            format!("FILL {} {}", render_locator(target), plain(value))
        }
        ActionKind::Type { target, text } => {
            format!("TYPE {} {}", render_locator(target), plain(text))
        }
        ActionKind::Press { target: None, key } => format!("PRESS {}", plain(key)),
        ActionKind::Press {
            target: Some(target),
            key,
        } => format!("PRESS {} {}", render_locator(target), plain(key)),
        ActionKind::Check { target } => format!("CHECK {}", render_locator(target)),
        ActionKind::Uncheck { target } => format!("UNCHECK {}", render_locator(target)),
        ActionKind::Select { target, option } => {
            format!("SELECT {} {}", render_locator(target), plain(option))
        }
        ActionKind::Hover { target } => format!("HOVER {}", render_locator(target)),
        ActionKind::Drag { source, target } => format!(
            "DRAG {} to {}",
            render_locator(source),
            render_locator(target)
        ),
        ActionKind::ScrollIntoView { target } => format!(
            "SCROLL {}",
            render_locator_from(target, ValueCtx::ScrollFirst)
        ),
        ActionKind::Scroll { target, motion } => {
            let motion = match motion {
                ScrollMotion::Chunk(direction) => direction.keyword().to_owned(),
                ScrollMotion::To(percent) => format!("to {percent}"),
            };
            match target {
                Some(target) => format!(
                    "SCROLL {} {motion}",
                    render_locator_from(target, ValueCtx::ScrollFirst)
                ),
                None => format!("SCROLL {motion}"),
            }
        }
        ActionKind::Upload { target, path } => format!(
            "UPLOAD {} file:{}",
            render_locator(target),
            render_value(path, ValueCtx::Prefixed)
        ),
        ActionKind::Drop { target, path } => format!(
            "DROP {} file:{}",
            render_locator(target),
            render_value(path, ValueCtx::Prefixed)
        ),
        ActionKind::Screenshot { name } => format!("SCREENSHOT {}", name.text),
        ActionKind::Snapshot { name, target, .. } => match target {
            Some(target) => format!("SNAPSHOT {} {}", name.text, render_locator(target)),
            None => format!("SNAPSHOT {}", name.text),
        },
        ActionKind::Eval { script } => format!("EVAL {}", plain(script)),
        ActionKind::Goal { goal } => format!("GOAL {}", plain(goal)),
        ActionKind::Act { scope, instruction } => match scope {
            Some(scope) => format!("ACT {} {}", render_locator(scope), plain(instruction)),
            None => format!("ACT {}", instruction_text(instruction)),
        },
        ActionKind::Extract {
            name,
            scope,
            instruction,
            ..
        } => match scope {
            Some(scope) => format!(
                "EXTRACT {} {} {}",
                name.text,
                render_locator(scope),
                plain(instruction)
            ),
            None => format!("EXTRACT {} {}", name.text, instruction_text(instruction)),
        },
        ActionKind::Store { scope, key, value } => {
            format!("STORE {} {} {}", scope.keyword(), plain(key), plain(value))
        }
    };
    push_timeout(&mut out, action.timeout);
    out
}

/// An instruction with no scope. A bare colon would start a scope, so
/// the value stays quoted then (SPEC 7.4).
fn instruction_text(instruction: &Value) -> String {
    render_value(instruction, ValueCtx::Unprefixed)
}

/// Renders a `PAGE` line (SPEC 8).
fn render_page(page: &Page) -> String {
    let mut out = match &page.check {
        PageCheck::Value(value) => format!("PAGE {}", render_value(value, ValueCtx::Page)),
        PageCheck::Matches(regex) => format!("PAGE matches {}", render_regex(regex)),
    };
    push_timeout(&mut out, page.timeout);
    out
}

fn state_check_text(state: StateCheck) -> &'static str {
    match state {
        StateCheck::Visible => "visible",
        StateCheck::Hidden => "hidden",
        StateCheck::Enabled => "enabled",
        StateCheck::Disabled => "disabled",
        StateCheck::Checked => "checked",
        StateCheck::Unchecked => "unchecked",
        StateCheck::Focused => "focused",
    }
}

/// True for a typed literal's spelling (SPEC 3.1): a JSON number,
/// `true`, `false`, `null`, or the shape of a bytes literal, which is bytes
/// or an error when bare.
fn is_typed_literal(text: &str) -> bool {
    Number::parse(text).is_some()
        || matches!(text, "true" | "false" | "null")
        || is_bytes_literal_shape(text)
}

/// Renders an expected value. Quotes stay on a value whose bare form is
/// a typed literal, and a JSON literal stays as written (SPEC 13).
fn render_operand(operand: &Operand) -> String {
    match operand {
        Operand::Json(literal) => literal.text.clone(),
        Operand::Value(value) => {
            // Quotes make a typed expected value a string (SPEC 9.6), so
            // they stay on a typed-literal lookalike such as `"42"` and on
            // any value with a variable, which could resolve to one.
            let keeps_quotes = value.quoted
                && value
                    .as_literal()
                    .is_none_or(|literal| is_typed_literal(&literal));
            if keeps_quotes {
                render_quoted(value)
            } else {
                render_value(value, ValueCtx::Operand)
            }
        }
    }
}

/// Renders `[not] predicate` (SPEC 9.4).
fn render_predicate(negated: bool, predicate: &PredicateSpec) -> String {
    let body = match predicate {
        PredicateSpec::Compare { kind, expected } => {
            format!("{} {}", kind.name(), render_operand(expected))
        }
        PredicateSpec::Matches(regex) => format!("matches {}", render_regex(regex)),
        PredicateSpec::Word(kind) => kind.name().to_owned(),
    };
    if negated { format!("not {body}") } else { body }
}

/// Renders one filter.
fn render_filter(filter: &FilterSpec) -> String {
    let mut out = filter.kind.name().to_owned();
    for arg in &filter.args {
        match arg {
            FilterArg::Value(value) if out.ends_with(':') => {
                out.push_str(&render_value(value, ValueCtx::Prefixed));
            }
            FilterArg::Value(value) => {
                out.push(' ');
                out.push_str(&render_value(value, ValueCtx::Plain));
            }
            FilterArg::Regex(regex) => {
                out.push(' ');
                out.push_str(&render_regex(regex));
            }
            FilterArg::Index(value) => {
                let _ = write!(out, " {value}");
            }
        }
    }
    out
}

fn render_extractor(extractor: &Extractor) -> String {
    match extractor {
        Extractor::Text => "text".to_owned(),
        Extractor::Value => "value".to_owned(),
        Extractor::Count => "count".to_owned(),
        Extractor::Attr(name) => format!("attr:{name}"),
    }
}

/// Renders a subject (SPEC 9.2).
fn render_subject(subject: &Subject) -> String {
    match subject {
        Subject::Element { locator, extractor } => format!(
            "{} {}",
            render_locator(locator),
            render_extractor(extractor)
        ),
        Subject::Url => "url".to_owned(),
        Subject::Title => "title".to_owned(),
        Subject::Eval(script) => format!("eval {}", render_value(script, ValueCtx::Plain)),
        Subject::Response { name: None, field } => render_response_field(field),
        Subject::Response {
            name: Some(name),
            field,
        } => format!("response:{} {}", name.text, render_response_field(field)),
        Subject::Request { name, field } => {
            format!("request:{} {}", name.text, render_request_field(field))
        }
        Subject::Extract { name, .. } => format!("extract:{}", name.text),
    }
}

fn render_request_field(field: &RequestField) -> String {
    match field {
        RequestField::Method => "method".to_owned(),
        RequestField::Url => "url".to_owned(),
        RequestField::Body => "body".to_owned(),
        RequestField::Bytes => "bytes".to_owned(),
        RequestField::Header(value) => {
            format!("header:{}", render_value(value, ValueCtx::Prefixed))
        }
        RequestField::Json(value) => format!("json:{}", render_value(value, ValueCtx::Prefixed)),
        RequestField::Xpath(value) => format!("xpath:{}", render_value(value, ValueCtx::Prefixed)),
    }
}

/// Renders `subject { filter }`.
fn render_chain(subject: &Subject, filters: &[FilterSpec]) -> String {
    let mut out = render_subject(subject);
    for filter in filters {
        out.push(' ');
        out.push_str(&render_filter(filter));
    }
    out
}

fn render_check(check: &CheckLine) -> String {
    format!(
        "{} {}",
        render_chain(&check.subject, &check.filters),
        render_predicate(check.negated, &check.predicate)
    )
}

/// Renders an `ASSERT` line (SPEC 9).
fn render_assert(assert: &Assert) -> String {
    let mut out = match &assert.body {
        AssertBody::WindowClosed { name } => format!("ASSERT window:{} closed", name.text),
        AssertBody::ElementState { locator, state } => format!(
            "ASSERT {} {}",
            render_locator(locator),
            state_check_text(*state)
        ),
        AssertBody::Check(check) => format!("ASSERT {}", render_check(check)),
    };
    push_timeout(&mut out, assert.timeout);
    out
}

fn render_response_field(field: &ResponseField) -> String {
    match field {
        ResponseField::Status => "status".to_owned(),
        ResponseField::Location => "location".to_owned(),
        ResponseField::Body => "body".to_owned(),
        ResponseField::Bytes => "bytes".to_owned(),
        ResponseField::Header(value) => {
            format!("header:{}", render_value(value, ValueCtx::Prefixed))
        }
        ResponseField::Json(value) => format!("json:{}", render_value(value, ValueCtx::Prefixed)),
        ResponseField::Xpath(value) => {
            format!("xpath:{}", render_value(value, ValueCtx::Prefixed))
        }
    }
}

/// Renders a `JUDGE` line (SPEC 9.8).
fn render_judge(judge: &Judge) -> String {
    let mut out = match &judge.scope {
        Some(scope) => format!(
            "JUDGE {} {}",
            render_locator(scope),
            render_value(&judge.claim, ValueCtx::Plain)
        ),
        None => format!("JUDGE {}", instruction_text(&judge.claim)),
    };
    push_timeout(&mut out, judge.timeout);
    out
}

/// Renders a `CAPTURE` line (SPEC 10).
fn render_capture(capture: &Capture) -> String {
    let chain = render_chain(&capture.subject, &capture.filters);
    let mut out = format!("CAPTURE {}: {chain}", capture.name.text);
    push_timeout(&mut out, capture.timeout);
    out
}

fn render_option_value<T>(value: &OptionValue<T>, literal: impl Fn(&T) -> String) -> String {
    match value {
        OptionValue::Literal(typed) => literal(typed),
        OptionValue::Interpolated(value) => render_value(value, ValueCtx::Plain),
    }
}

fn viewport_text(viewport: Viewport) -> String {
    format!("{}x{}", viewport.width, viewport.height)
}

/// Renders a locator for reports and the AI cache, such as a `SNAPSHOT`
/// target or a generated locator (SPEC 6, 12.1).
pub fn render_snapshot_target(target: &Locator) -> String {
    render_locator(target)
}

/// Renders a snapshot setting at either scope (SPEC 5, 7).
pub fn render_snapshot_option(option: &SnapshotOption) -> String {
    let value = match option {
        SnapshotOption::Mask(None) => "none".to_owned(),
        SnapshotOption::Mask(Some(locator)) => render_locator(locator),
        SnapshotOption::MaxDiff(value) => render_option_value(value, ToString::to_string),
        SnapshotOption::PixelThreshold(value) => render_option_value(value, ToString::to_string),
    };
    format!("{}: {value}", option.key())
}

fn render_option(option: &FileOption) -> String {
    let plain = |value: &Value| render_value(value, ValueCtx::Plain);
    match option {
        FileOption::Snapshot(option) => render_snapshot_option(option),
        FileOption::AppUrl(value) => format!("app-url: {}", plain(value)),
        FileOption::Browser(value) => format!(
            "browser: {}",
            render_option_value(value, |browser| browser.as_str().to_owned())
        ),
        FileOption::Viewport(value) => format!(
            "viewport: {}",
            render_option_value(value, |viewport| viewport_text(*viewport))
        ),
        FileOption::StepTimeout(value) => format!(
            "step-timeout: {}",
            render_option_value(value, |duration| render_duration(*duration))
        ),
        FileOption::EntryTimeout(value) => format!(
            "entry-timeout: {}",
            render_option_value(value, |duration| render_duration(*duration))
        ),
        FileOption::NavTimeout(value) => format!(
            "nav-timeout: {}",
            render_option_value(value, |duration| render_duration(*duration))
        ),
        FileOption::AllowHosts(globs) | FileOption::BlockHosts(globs) => {
            let globs: Vec<String> = globs.iter().map(plain).collect();
            format!("{}: {}", option.key(), globs.join(" "))
        }
        FileOption::Dialogs(value) => format!(
            "dialogs: {}",
            render_option_value(value, |policy| policy.as_str().to_owned())
        ),
        FileOption::ReducedMotion(value) => format!(
            "reduced-motion: {}",
            render_option_value(value, |motion| motion.as_str().to_owned())
        ),
        FileOption::Storage(value) => format!("storage: {}", plain(value)),
        FileOption::UserAgent(value) => format!("user-agent: {}", plain(value)),
        FileOption::Setup(value) => format!("setup: {}", plain(value)),
        FileOption::Model(value) => format!("model: {}", plain(value)),
        FileOption::BrowserSimOrigin(value) => format!(
            "browsersim-origin: {}",
            render_option_value(value, |origin| origin.as_str().to_owned())
        ),
    }
}

/// One output line before comment interleaving: its original source line
/// number and its canonical text.
struct Line {
    source_line: u32,
    text:        String,
}

/// A run of lines separated from its neighbors by one blank line: the
/// `[Options]` section or one entry. Comments join the region of the
/// next content line below them, so an entry's naming comments stay
/// directly above it.
struct Region {
    lines: Vec<Line>,
}

fn option_region(file: &File) -> Option<Region> {
    let header = file.options_header?;
    let mut lines = vec![Line {
        source_line: header,
        text:        "[Options]".to_owned(),
    }];
    for option in &file.options {
        lines.push(Line {
            source_line: option.line,
            text:        render_option(&option.option),
        });
    }
    Some(Region { lines })
}

fn entry_region(entry: &Entry) -> Region {
    let mut lines = Vec::new();
    for action in &entry.actions {
        lines.push(Line {
            source_line: action.line,
            text:        render_action(action),
        });
        if let ActionKind::Snapshot { options, .. } = &action.kind {
            for option in options {
                lines.push(Line {
                    source_line: option.line,
                    text:        render_snapshot_option(&option.option),
                });
            }
        }
        let request_lines = match &action.kind {
            ActionKind::Http { headers, body, .. }
            | ActionKind::Mock {
                response: MockResponse::Fulfill { headers, body, .. },
                ..
            } => Some((headers, body)),
            _ => None,
        };
        if let ActionKind::Extract {
            schema: Some(schema),
            ..
        } = &action.kind
        {
            for (offset, text) in schema.text.split('\n').enumerate() {
                lines.push(Line {
                    source_line: schema.line + u32::try_from(offset).unwrap_or(u32::MAX),
                    text:        text.to_owned(),
                });
            }
        }
        if let Some((headers, body)) = request_lines {
            for header in headers {
                lines.push(Line {
                    source_line: header.line,
                    text:        format!(
                        "{}: {}",
                        header.name,
                        render_value(&header.value, ValueCtx::Plain)
                    ),
                });
            }
            if let Some(body) = body {
                match body.kind {
                    HttpBodyKind::Json => {
                        for (offset, text) in body.text.split('\n').enumerate() {
                            lines.push(Line {
                                source_line: body.line + u32::try_from(offset).unwrap_or(u32::MAX),
                                text:        text.to_owned(),
                            });
                        }
                    }
                    HttpBodyKind::Text => {
                        lines.push(Line {
                            source_line: body.line,
                            text:        "```".to_owned(),
                        });
                        if !body.text.is_empty() {
                            for (offset, text) in body.text.split('\n').enumerate() {
                                lines.push(Line {
                                    source_line: body.line
                                        + 1
                                        + u32::try_from(offset).unwrap_or(u32::MAX),
                                    text:        text.to_owned(),
                                });
                            }
                        }
                        lines.push(Line {
                            source_line: body.end_line,
                            text:        "```".to_owned(),
                        });
                    }
                }
            }
        }
    }
    if let Some(page) = &entry.page {
        lines.push(Line {
            source_line: page.line,
            text:        render_page(page),
        });
    }
    for check in &entry.checks {
        let text = match check {
            CheckStep::Assert(assert) => render_assert(assert),
            CheckStep::Judge(judge) => render_judge(judge),
            CheckStep::Capture(capture) => render_capture(capture),
        };
        lines.push(Line {
            source_line: check.line(),
            text,
        });
    }
    Region { lines }
}

/// Inserts each full-line comment into the region holding the next
/// content line below it (trailing comments join the last region), then
/// restores source order inside each region.
fn place_comments(regions: &mut [Region], comments: &[Comment]) {
    for comment in comments.iter().filter(|comment| comment.own_line) {
        let index = regions
            .iter()
            .position(|region| {
                region
                    .lines
                    .iter()
                    .any(|line| line.source_line > comment.line)
            })
            .or_else(|| regions.len().checked_sub(1));
        let Some(region) = index.and_then(|index| regions.get_mut(index)) else {
            continue;
        };
        region.lines.push(Line {
            source_line: comment.line,
            text:        format!("#{}", comment.text),
        });
    }
    for region in regions {
        region.lines.sort_by_key(|line| line.source_line);
    }
}

/// Renders a parsed file in canonical form (SPEC 13). The result ends
/// with a newline; re-parsing it yields a structurally identical file.
pub fn format_file(file: &File) -> String {
    let mut regions: Vec<Region> = Vec::new();
    if let Some(region) = option_region(file) {
        regions.push(region);
    }
    for entry in &file.entries {
        regions.push(entry_region(entry));
    }
    place_comments(&mut regions, &file.comments);
    let inline: Vec<&Comment> = file
        .comments
        .iter()
        .filter(|comment| !comment.own_line)
        .collect();
    let mut out = String::new();
    for (index, region) in regions.iter().enumerate() {
        if index > 0 {
            out.push('\n');
        }
        for line in &region.lines {
            out.push_str(&line.text);
            if let Some(comment) = inline
                .iter()
                .find(|comment| comment.line == line.source_line)
            {
                let _ = write!(out, " #{}", comment.text);
            }
            out.push('\n');
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;
    use crate::ast::snapshot::SnapshotOption;
    use crate::ast::{Ident, LocatorSegment, OptionLine, Span};
    use crate::parse::parse_file;

    // ---- Structural comparison -------------------------------------
    //
    // `parse(format(f))` must equal `parse(f)` up to source positions,
    // raw step text, and quoting style. The scrubbers below zero those
    // fields so `assert_eq!` compares only structure.

    const ZERO: Span = Span {
        line:   0,
        column: 0,
        len:    0,
    };

    fn scrub_value(value: &mut Value) {
        value.span = ZERO;
        value.quoted = false;
    }

    fn scrub_regex(regex: &mut Regex) {
        regex.span = ZERO;
    }

    fn scrub_ident(ident: &mut Ident) {
        ident.span = ZERO;
    }

    fn scrub_locator(locator: &mut Locator) {
        locator.span = ZERO;
        for segment in &mut locator.segments {
            scrub_segment(segment);
        }
    }

    fn scrub_segment(segment: &mut LocatorSegment) {
        segment.span = ZERO;
        match &mut segment.kind {
            SegmentKind::Role { name, .. } => {
                if let Some(name) = name {
                    scrub_value(name);
                }
            }
            SegmentKind::TextEngine { value, .. }
            | SegmentKind::TestId(value)
            | SegmentKind::Css(value)
            | SegmentKind::Frame(value)
            | SegmentKind::Default(value)
            | SegmentKind::Ai(value) => scrub_value(value),
            SegmentKind::Nth(_) | SegmentKind::Ref(_) => {}
        }
    }

    fn scrub_subject(subject: &mut Subject) {
        match subject {
            Subject::Element { locator, .. } => scrub_locator(locator),
            Subject::Eval(script) => scrub_value(script),
            Subject::Response { name, field } => {
                if let Some(name) = name {
                    scrub_ident(name);
                }
                scrub_response_field(field);
            }
            Subject::Request { name, field } => {
                scrub_ident(name);
                if let RequestField::Header(value)
                | RequestField::Json(value)
                | RequestField::Xpath(value) = field
                {
                    scrub_value(value);
                }
            }
            Subject::Extract { name, .. } => scrub_ident(name),
            Subject::Url | Subject::Title => {}
        }
    }

    fn scrub_filters(filters: &mut [FilterSpec]) {
        for filter in filters {
            filter.span = ZERO;
            for arg in &mut filter.args {
                match arg {
                    FilterArg::Value(value) => scrub_value(value),
                    FilterArg::Regex(regex) => scrub_regex(regex),
                    FilterArg::Index(_) => {}
                }
            }
        }
    }

    fn scrub_predicate(predicate: &mut PredicateSpec) {
        match predicate {
            PredicateSpec::Compare { expected, .. } => match expected {
                Operand::Value(value) => scrub_value(value),
                Operand::Json(literal) => scrub_value(&mut literal.value),
            },
            PredicateSpec::Matches(regex) => scrub_regex(regex),
            PredicateSpec::Word(_) => {}
        }
    }

    fn scrub_option_value<T>(value: &mut OptionValue<T>) {
        if let OptionValue::Interpolated(value) = value {
            scrub_value(value);
        }
    }

    fn scrub_snapshot_option(option: &mut SnapshotOption) {
        match option {
            SnapshotOption::Mask(Some(locator)) => scrub_locator(locator),
            SnapshotOption::Mask(None) => {}
            SnapshotOption::MaxDiff(value) => scrub_option_value(value),
            SnapshotOption::PixelThreshold(value) => scrub_option_value(value),
        }
    }

    fn scrub_option_line(option: &mut OptionLine) {
        option.line = 0;
        option.span = ZERO;
        match &mut option.option {
            FileOption::Snapshot(value) => scrub_snapshot_option(value),
            FileOption::AppUrl(value)
            | FileOption::Storage(value)
            | FileOption::UserAgent(value)
            | FileOption::Setup(value)
            | FileOption::Model(value) => {
                scrub_value(value);
            }
            FileOption::Browser(value) => scrub_option_value(value),
            FileOption::Viewport(value) => scrub_option_value(value),
            FileOption::StepTimeout(value)
            | FileOption::EntryTimeout(value)
            | FileOption::NavTimeout(value) => scrub_option_value(value),
            FileOption::Dialogs(value) => scrub_option_value(value),
            FileOption::ReducedMotion(value) => scrub_option_value(value),
            FileOption::BrowserSimOrigin(value) => scrub_option_value(value),
            FileOption::AllowHosts(globs) | FileOption::BlockHosts(globs) => {
                for glob in globs {
                    scrub_value(glob);
                }
            }
        }
    }

    fn scrub_action(action: &mut Action) {
        action.line = 0;
        action.span = ZERO;
        action.text = String::new();
        match &mut action.kind {
            ActionKind::Http {
                url,
                headers,
                body,
                source,
                ..
            }
            | ActionKind::Mock {
                url,
                response: MockResponse::Fulfill { headers, body, .. },
                source,
                ..
            } => {
                source.clear();
                scrub_value(url);
                for header in headers {
                    header.line = 0;
                    scrub_value(&mut header.value);
                }
                if let Some(body) = body {
                    body.line = 0;
                    body.end_line = 0;
                    scrub_value(&mut body.value);
                }
            }
            ActionKind::Response { name, url, .. } => {
                scrub_ident(name);
                scrub_value(url);
            }
            ActionKind::Visit { url } => scrub_value(url),
            ActionKind::Mock {
                url,
                response: MockResponse::Failed,
                source,
                ..
            } => {
                source.clear();
                scrub_value(url);
            }
            ActionKind::Click { target, .. }
            | ActionKind::Dblclick { target }
            | ActionKind::Check { target }
            | ActionKind::Uncheck { target }
            | ActionKind::Hover { target }
            | ActionKind::ScrollIntoView { target } => scrub_locator(target),
            ActionKind::Fill { target, value }
            | ActionKind::Type {
                target,
                text: value,
            }
            | ActionKind::Select {
                target,
                option: value,
            }
            | ActionKind::Upload {
                target,
                path: value,
            }
            | ActionKind::Drop {
                target,
                path: value,
            } => {
                scrub_locator(target);
                scrub_value(value);
            }
            ActionKind::Drag { source, target } => {
                scrub_locator(source);
                scrub_locator(target);
            }
            ActionKind::Scroll { target, .. } => {
                if let Some(target) = target {
                    scrub_locator(target);
                }
            }
            ActionKind::Press { target, key } => {
                if let Some(target) = target {
                    scrub_locator(target);
                }
                scrub_value(key);
            }
            ActionKind::Popup { name }
            | ActionKind::Window { name }
            | ActionKind::Close { name }
            | ActionKind::Screenshot { name } => scrub_ident(name),
            ActionKind::Snapshot {
                name,
                target,
                options,
            } => {
                scrub_ident(name);
                if let Some(target) = target {
                    scrub_locator(target);
                }
                for option in options {
                    option.line = 0;
                    option.span = ZERO;
                    scrub_snapshot_option(&mut option.option);
                }
            }
            ActionKind::Eval { script } | ActionKind::Goal { goal: script } => scrub_value(script),
            ActionKind::Act { scope, instruction } => {
                if let Some(scope) = scope {
                    scrub_locator(scope);
                }
                scrub_value(instruction);
            }
            ActionKind::Extract {
                name,
                scope,
                instruction,
                schema,
            } => {
                scrub_ident(name);
                if let Some(scope) = scope {
                    scrub_locator(scope);
                }
                scrub_value(instruction);
                if let Some(schema) = schema {
                    schema.line = 0;
                    schema.end_line = 0;
                }
            }
            ActionKind::Store { key, value, .. } => {
                scrub_value(key);
                scrub_value(value);
            }
        }
    }

    fn scrub_response_field(field: &mut ResponseField) {
        match field {
            ResponseField::Status
            | ResponseField::Location
            | ResponseField::Body
            | ResponseField::Bytes => {}
            ResponseField::Header(value)
            | ResponseField::Json(value)
            | ResponseField::Xpath(value) => scrub_value(value),
        }
    }

    fn scrub_entry(entry: &mut Entry) {
        for action in &mut entry.actions {
            scrub_action(action);
        }
        if let Some(page) = &mut entry.page {
            page.line = 0;
            page.span = ZERO;
            page.text = String::new();
            match &mut page.check {
                PageCheck::Value(value) => scrub_value(value),
                PageCheck::Matches(regex) => scrub_regex(regex),
            }
        }
        for check in &mut entry.checks {
            match check {
                CheckStep::Assert(assert) => scrub_assert(assert),
                CheckStep::Judge(judge) => {
                    judge.line = 0;
                    judge.span = ZERO;
                    judge.text = String::new();
                    if let Some(scope) = &mut judge.scope {
                        scrub_locator(scope);
                    }
                    scrub_value(&mut judge.claim);
                }
                CheckStep::Capture(capture) => scrub_capture(capture),
            }
        }
    }

    fn scrub_assert(assert: &mut Assert) {
        assert.line = 0;
        assert.span = ZERO;
        assert.text = String::new();
        match &mut assert.body {
            AssertBody::WindowClosed { name } => scrub_ident(name),
            AssertBody::ElementState { locator, .. } => scrub_locator(locator),
            AssertBody::Check(check) => {
                scrub_subject(&mut check.subject);
                scrub_filters(&mut check.filters);
                scrub_predicate(&mut check.predicate);
            }
        }
    }

    fn scrub_capture(capture: &mut Capture) {
        capture.line = 0;
        capture.span = ZERO;
        capture.text = String::new();
        scrub_ident(&mut capture.name);
        scrub_subject(&mut capture.subject);
        scrub_filters(&mut capture.filters);
    }

    fn scrub_file(mut file: File) -> File {
        file.options_header = file.options_header.map(|_| 0);
        for option in &mut file.options {
            scrub_option_line(option);
        }
        for entry in &mut file.entries {
            scrub_entry(entry);
        }
        for comment in &mut file.comments {
            comment.line = 0;
            comment.column = 0;
        }
        file
    }

    // ---- Helpers ---------------------------------------------------

    fn parse(source: &str) -> File {
        parse_file(Path::new("test.whirl"), source)
            .unwrap_or_else(|error| panic!("fixture should parse:\n{error}"))
    }

    fn fmt(source: &str) -> String {
        format_file(&parse(source))
    }

    /// Formatting preserves structure and is idempotent.
    fn assert_round_trip(source: &str) {
        let parsed = parse(source);
        let formatted = format_file(&parsed);
        let reparsed = parse_file(Path::new("test.whirl"), &formatted)
            .unwrap_or_else(|error| panic!("formatted output should parse:\n{formatted}\n{error}"));
        assert_eq!(
            scrub_file(reparsed.clone()),
            scrub_file(parsed),
            "formatting changed the parse; formatted:\n{formatted}"
        );
        assert_eq!(
            format_file(&reparsed),
            formatted,
            "formatting is not idempotent"
        );
    }

    #[test]
    fn act_round_trips_with_and_without_a_scope() {
        assert_round_trip("VISIT /\nACT \"add the first product to the cart\" @60s\n");
        assert_round_trip("VISIT /\nACT css:form >> group:* \"click Buy\"\n");
        assert_eq!(
            fmt("VISIT /\nACT   css:form   \"click Buy\"\n"),
            "VISIT /\nACT css:form \"click Buy\"\n"
        );
    }

    // ---- Fixtures --------------------------------------------------

    /// Valid sources covering every construct; each must round-trip.
    const FIXTURES: [&str; 21] = [
        // The SPEC section 2 example.
        "# checkout.whirl \u{2014} buy a widget as a signed-in user.\n[Options]\napp-url: https://shop.example.com\nviewport: 1280x800\n\n# Log in.\nVISIT /login\n\nFILL \"Email\" alice@example.com\nFILL \"Password\" {{env.TEST_PASSWORD}}\nCLICK button:\"Sign in\"\nPAGE /dashboard\nASSERT heading:\"Welcome back\" visible\nASSERT testid:user-menu text == Alice\n\n# Find a product.\nFILL placeholder:\"Search products\" widget\nPRESS Enter\nASSERT url contains \"q=widget\"\nASSERT testid:result-card count >= 1\nCAPTURE first_product: testid:result-card >> nth:1 >> link:* attr:href\n\n# Add it to the cart.\nVISIT {{first_product}}\nCLICK \"Add to cart\"\nASSERT testid:cart-badge text == 1\nASSERT alert:* text contains \"Added to cart\"\n",
        // Every option key, including interpolated values.
        "[Options]\napp-url: https://example.com\nbrowser: webkit\nviewport: 800x600\nstep-timeout: 5s\nentry-timeout: 90s\nnav-timeout: 45s\nallow-hosts: example.com *.example.com\ndialogs: accept\nreduced-motion: reduce\nstorage: auth/state.json\nuser-agent: \"Mozilla/5.0 (Whirl)\"\nsetup: sign-in.whirl\nVISIT /\n",
        "[Options]\nbrowser: {{engine}}\nviewport: {{size}}\nstep-timeout: {{t}}\nVISIT /\n",
        // Every action form.
        "VISIT /a\nCLICK \"Add to cart\"\nRIGHTCLICK \"report.pdf\"\nMIDDLECLICK link:Docs\nDBLCLICK text:~\"added\"\nFILL \"Email\" alice@example.com\nTYPE \"Code\" 424242\nPRESS Enter\nPRESS label:Search \"Control+A\"\nCHECK \"Remember me\"\nUNCHECK checkbox:\"Spam\"\nSELECT \"Country\" \"United States\"\nHOVER testid:menu\nDRAG \"Write spec\" to testid:done\nDRAG \"to\" to listitem:\"to\"\nSCROLL testid:feed\nSCROLL down\nSCROLL dialog:Filters up\nSCROLL to 50%\nSCROLL testid:board to 33.5%\nSCROLL \"down\"\nSCROLL \"to\" left\nUPLOAD \"Avatar\" file:images/cat.png\nDROP \"Drop files here\" file:reports/q3.csv\nDROP testid:dropzone file:{{report}}\nSCREENSHOT overview\nSNAPSHOT header\nEVAL \"window.scrollTo(0, 0)\"\nSTORE local onboarding:done yes\nSTORE local \"welcome seen\" {{env.SEEN}}\nSTORE session draft hi\nSTORE cookie chat_version v1\nVISIT /u/{{setup.user_id}}\n",
        // Timeout suffixes on every step kind.
        "VISIT / @45s\nCLICK go @60s\nPAGE /done @2s\nASSERT testid:x visible @2500ms\nASSERT url == / @1s\nCAPTURE n: testid:x text @3s\nCAPTURE m: testid:x text regex /x(y)?/ @3s\n",
        // Every assert form and operator.
        "VISIT /\nASSERT testid:a visible\nASSERT testid:a hidden\nASSERT testid:a enabled\nASSERT testid:a disabled\nASSERT testid:a checked\nASSERT testid:a unchecked\nASSERT testid:a focused\nASSERT testid:a text == x\nASSERT testid:a text != x\nASSERT testid:a text contains x\nASSERT testid:a text matches /Order #\\w+/i\nASSERT testid:a value == 0\nASSERT testid:a attr:aria-expanded == true\nASSERT testid:a attr:data-state != open\nASSERT testid:a count == 3\nASSERT testid:a count != 3\nASSERT testid:a count < 3\nASSERT testid:a count <= 3\nASSERT testid:a count > 3\nASSERT testid:a count >= 3\nASSERT url == https://x/\nASSERT url matches /a.b/ism\nASSERT title contains Check\n",
        // Every capture form.
        "VISIT /\nCAPTURE a: testid:x text\nCAPTURE b: label:Amount value\nCAPTURE c: css:\".row\" count\nCAPTURE d: link:\"Docs\" attr:href\nCAPTURE e: url\nCAPTURE f: title\nCAPTURE g: eval \"document.title\"\nCAPTURE h: testid:x text regex /Order #(\\w+)/\n",
        // Locator shapes: chains, nth, every prefix and substring form.
        "VISIT /\nCLICK button:\"Sign in\"\nCLICK button:~\"sign\"\nCLICK label:Email >> nth:2\nCLICK label:~mail\nCLICK placeholder:Search\nCLICK placeholder:~sea\nCLICK text:Go\nCLICK text:~go\nCLICK alt:Logo\nCLICK alt:~logo\nCLICK title:Info\nCLICK title:~info\nCLICK testid:cart >> css:\".x > .y\" >> nth:1\n",
        // Quotes the parse depends on.
        "VISIT /\nCLICK \"css:foo\"\nCLICK \"role:button\"\nCLICK \"nth:2\"\nCLICK \">>\"\nFILL Email \"@60s\"\nFILL Email \"@60x\"\nPAGE \"matches\"\nASSERT button:\"visible\" visible\nASSERT button:\"count\" text == \"@5s\"\nCAPTURE x: link:\"text\" text\n",
        // Values that are safe to bare.
        "VISIT \"/dashboard\"\nFILL \"Email\" \"alice\"\nPAGE \"/x\"\nASSERT url == \"q\"\n",
        // Escapes and interpolation.
        "VISIT /\nFILL \"Says \\\"hi\\\"\" \"a\\tb\\nc\\\\d\"\nFILL \"U\" \"\\u{1F600}ok\"\nFILL \"B\" \"\\{{literal\"\nVISIT a\\{{b\nVISIT {{base_url}}/next\nFILL \"P\" {{env.SECRET}}\n",
        // Comments everywhere.
        "# top\n[Options] # inline options\napp-url: https://x # inline app-url\n\n# name entry one\nVISIT / # go\n# between actions\nCLICK x\nPAGE / # landed\n# before check\nASSERT url == / # eq\n# before cap\nCAPTURE c: url # cap\n\n# name entry two\nVISIT /two\n# trailing comment\n",
        // Blank-line and spacing noise.
        "\n\n[Options]\n\n\napp-url:    https://x\n\n\nVISIT     /\n\n\nPAGE      /\n\n\n\nVISIT   /b\n\n",
        // CRLF line endings.
        "VISIT /\r\nPAGE /\r\nASSERT url == /\r\n",
        // Empty sections keep their headers.
        "VISIT /\n",
        "VISIT /\n",
        "[Options]\nVISIT /\n",
        // PRESS one-argument vs two-argument forms.
        "VISIT /\nPRESS Enter\nPRESS \"Control+A\"\nPRESS label:Search Enter\nPRESS textbox:\"Query\" Enter\n",
        // Default-engine values that stay bare.
        "VISIT /\nCLICK Save\nFILL Email alice\nUPLOAD Avatar file:cat.png\nDROP Dropzone file:cat.png\nDROP \"file:zone\" file:cat.png\nSELECT Country France\n",
        // Attached prefix values that need quotes.
        "VISIT /\nCLICK css:\".a .b\" >> text:\"Add to cart\"\nCLICK label:\"First name\"\nUPLOAD \"Avatar\" file:\"my cat.png\"\nDROP \"Drop files here\" file:\"my cat.png\"\nDROP \"css:.zone\" file:\"@5s\"\n",
        // Capture names and eval edge spellings.
        "VISIT /\nCAPTURE a_1: eval \"1 + 1\"\nCAPTURE b: eval regex\nCAPTURE c: eval \"@5s\"\nCAPTURE d: eval x regex /y/\n",
    ];

    #[test]
    fn every_fixture_round_trips() {
        for fixture in FIXTURES {
            assert_round_trip(fixture);
        }
    }

    #[test]
    fn http_round_trips_headers_bodies_and_timeout_shaped_values() {
        assert_round_trip(
            r#"VISIT /
HTTP POST /api/orders @5s
Authorization: "Bearer {{env.API_KEY}}"
Content-Type: application/json
{"name":"Ada"}
ASSERT status == 201
HTTP GET /
X-Value: "@10s"
HTTP GET "@10s"
"#,
        );
    }

    #[test]
    fn extract_round_trips_with_its_schema_as_written() {
        assert_round_trip(
            "VISIT /\nEXTRACT order   css:main  \"the total\" @30s\n{\n    \"type\":  \"number\"\n}\nASSERT extract:order > 0\n",
        );
        assert_eq!(
            fmt("VISIT /\nEXTRACT order   \"the total\"\n{ \"type\":  \"number\" }\n"),
            "VISIT /\nEXTRACT order \"the total\"\n{ \"type\":  \"number\" }\n"
        );
    }

    #[test]
    fn goal_round_trips() {
        assert_round_trip("VISIT /\nGOAL \"buy {{item}}\" @180s\nASSERT url exists\n");
        assert_eq!(
            fmt("VISIT /\nGOAL    \"buy a mug\"\nASSERT url exists\n"),
            "VISIT /\nGOAL \"buy a mug\"\nASSERT url exists\n"
        );
    }

    #[test]
    fn judge_round_trips() {
        assert_round_trip(
            "VISIT /\nASSERT testid:x visible\nJUDGE testid:x \"the total is {{total}}\" @20s\nJUDGE \"no error shows\"\n",
        );
        assert_eq!(
            fmt("VISIT /\nJUDGE    css:main   \"it is fine\"\n"),
            "VISIT /\nJUDGE css:main \"it is fine\"\n"
        );
    }

    #[test]
    fn ai_targets_round_trip() {
        assert_round_trip(
            "VISIT /\nCLICK dialog:* >> ai:\"the second email field\"\nASSERT ai:total text == 1\nCAPTURE t: ai:\"the {{x}} total\" text\n",
        );
        assert_eq!(
            fmt("VISIT /\nCLICK ai:\"buy\"\n"),
            "VISIT /\nCLICK ai:buy\n"
        );
    }

    #[test]
    fn collapses_spacing_to_single_spaces() {
        assert_eq!(
            fmt("VISIT    /login\nCLICK   button:\"Sign in\"   @5s\n"),
            "VISIT /login\nCLICK button:\"Sign in\" @5s\n"
        );
    }

    #[test]
    fn removes_quotes_a_bare_spelling_preserves() {
        assert_eq!(
            fmt("VISIT \"/dashboard\"\nFILL \"Email\" \"alice\"\nASSERT url == \"q\"\n"),
            "VISIT /dashboard\nFILL Email alice\nASSERT url == q\n"
        );
    }

    #[test]
    fn keeps_quotes_on_typed_literal_spellings() {
        assert_eq!(
            fmt(
                "VISIT /\nASSERT testid:x text == \"1\"\nASSERT url == \"true\"\nASSERT url != \"paid\"\nASSERT eval \"1\" == 1\nASSERT eval \"1\" == \"[a]\"\n"
            ),
            "VISIT /\nASSERT testid:x text == \"1\"\nASSERT url == \"true\"\nASSERT url != paid\nASSERT eval 1 == 1\nASSERT eval 1 == \"[a]\"\n"
        );
    }

    #[test]
    fn keeps_quotes_on_interpolated_expected_values() {
        let source = "HTTP GET /x\nASSERT status == 200\nASSERT json:$.id == \"{{order_id}}\"\nASSERT json:$.n == \"{{a}}1\"\nASSERT json:$.id == {{order_id}}\n";
        assert_eq!(fmt(source), source);
    }

    #[test]
    fn keeps_json_literals_as_written() {
        let source = "VISIT /\nRESPONSE r GET /x\nASSERT response:r json:$.a == {\"b\":  [1,2]}\n";
        assert_eq!(fmt(source), source);
    }

    #[test]
    fn quotes_values_that_require_them() {
        assert_eq!(
            fmt("VISIT /\nCLICK \"Add   to cart\"\nFILL Email \"a\\\"b\"\n"),
            "VISIT /\nCLICK \"Add   to cart\"\nFILL Email \"a\\\"b\"\n"
        );
    }

    #[test]
    fn scroll_keeps_quotes_on_words_it_reads_as_its_motion() {
        assert_eq!(
            fmt("VISIT /\nSCROLL \"feed\" down\nSCROLL \"right\"\nSCROLL region:\"up\" to 100%\n"),
            "VISIT /\nSCROLL feed down\nSCROLL \"right\"\nSCROLL region:up to 100%\n"
        );
    }

    #[test]
    fn drag_needs_no_quotes_on_a_to_that_is_text() {
        // DRAG reads left to right, so only the `to` after the first
        // locator separates them (SPEC 7).
        assert_eq!(
            fmt("VISIT /\nDRAG \"Card\" to \"Done\"\nDRAG \"to\" to region:\"to\"\nCLICK \"to\"\n"),
            "VISIT /\nDRAG Card to Done\nDRAG to to region:to\nCLICK to\n"
        );
    }

    #[test]
    fn drop_renders_like_upload() {
        assert_eq!(
            fmt(
                "VISIT /\nDROP   \"Drop files here\"   file:\"q3.csv\"   @5s\nDROP \"css:.zone\" file:\"my q3.csv\"\n"
            ),
            "VISIT /\nDROP \"Drop files here\" file:q3.csv @5s\nDROP \"css:.zone\" file:\"my q3.csv\"\n"
        );
    }

    #[test]
    fn responses_round_trip_with_fields_captures_and_interpolation() {
        assert_round_trip(
            r#"VISIT /
CLICK Order
RESPONSE order POST /api/orders @30s
ASSERT response:order status >= 200
ASSERT response:order header:Content-Type contains application/json
ASSERT response:order json:"$['key with space']" == true
ASSERT response:order json:{{path}} matches /paid/i
ASSERT response:order json:$.total toFloat >= 1e3
ASSERT response:order json:$.tags == ["a", "{{tag}}"]
ASSERT response:order json:$.id == "42"
ASSERT response:order json:$.items[*].sku not contains ABC-1
ASSERT response:order bytes startsWith hex,7b;
ASSERT response:order location urlQueryParam next == /cart
ASSERT response:order xpath:"count(//li[@class='row'])" == 3
ASSERT response:order xpath://_:entry count >= 1
ASSERT response:order body xpath:"string(//h1)" == Checks
ASSERT response:order xpath:{{expr}} exists
CAPTURE body: response:order json:$
CAPTURE id: response:order json:$.id toString regex /order-(\d+)/ @2s
CAPTURE status: response:order status
CAPTURE header: response:order header:{{header_name}}
"#,
        );
    }

    #[test]
    fn named_tabs_round_trip() {
        assert_round_trip(
            "VISIT /\nCLICK Pay\nPOPUP payment @30s\nWINDOW payment\nCLOSE payment\nASSERT window:payment closed @5s\nWINDOW main\n",
        );
    }

    #[test]
    fn frame_locators_round_trip_in_actions_asserts_and_captures() {
        assert_round_trip(
            r##"VISIT /
CLICK "frame:literal"
FILL frame:"#payment iframe" >> nth:2 >> frame:iframe >> label:Email alice@example.com
ASSERT frame:"#payment iframe" >> label:Email value == alice@example.com
CAPTURE email: frame:"#payment iframe" >> label:Email value
"##,
        );
    }

    #[test]
    fn snapshot_options_preserve_comments_order_units_and_meaning() {
        let source = "[Options]\nsnapshot-mask: testid:clock\nsnapshot-max-diff: 0.125%\n\nVISIT /\nSNAPSHOT first @2s # headline\n# local masks\nsnapshot-mask: button:\"Buy now\" # inline\nsnapshot-mask: frame:iframe >> css:.price\nsnapshot-pixel-threshold: 2e-1\nSNAPSHOT second\nsnapshot-mask: none\nsnapshot-max-diff: 0\n";
        assert_round_trip(source);
        assert_eq!(fmt(source), source);
    }

    #[test]
    fn snapshot_targets_round_trip_with_options_and_timeouts() {
        let source = "[Options]\nsnapshot-mask: testid:clock\n\nVISIT /\nSNAPSHOT cart testid:cart @10s\nsnapshot-mask: testid:cart >> testid:delivery-estimate\nsnapshot-max-diff: 0.5%\nSNAPSHOT payment frame:\"#payment iframe\" >> testid:payment-form\nsnapshot-mask: none\nSNAPSHOT buy button:\"Buy now\" >> nth:0\nSNAPSHOT row testid:{{row_id}}\nSNAPSHOT total text:\"Order total\"\n";
        assert_round_trip(source);
        assert_eq!(fmt(source), source);
        assert_eq!(
            fmt("VISIT /\nSNAPSHOT   cart   css:\".cart\"   @2s\n"),
            "VISIT /\nSNAPSHOT cart css:.cart @2s\n"
        );
    }

    #[test]
    fn keeps_quotes_on_prefix_shaped_default_segments() {
        let source = "VISIT /\nCLICK \"css:foo\"\nCLICK \"role:button\"\nCLICK \"nth:2\"\n";
        assert_eq!(fmt(source), source);
    }

    #[test]
    fn keeps_quotes_on_values_that_would_read_as_a_substring_marker() {
        let source = "VISIT /\nCLICK button:\"~x\"\nCLICK text:\"~x\"\nCLICK button:~\"*\"\nCLICK button:~~x\nCLICK css:~x\n";
        assert_eq!(fmt(source), source);
        assert_eq!(
            fmt("VISIT /\nCLICK button:~\"Sign\"\nCLICK label:~\"mail\"\n"),
            "VISIT /\nCLICK button:~Sign\nCLICK label:~mail\n"
        );
    }

    #[test]
    fn a_hash_inside_a_value_needs_no_quotes() {
        assert_eq!(
            fmt("VISIT \"/docs#install\"\nCLICK css:\"#submit\"\nFILL Note \"#1\" # a note\n"),
            "VISIT /docs#install\nCLICK css:#submit\nFILL Note \"#1\" # a note\n"
        );
    }

    #[test]
    fn keeps_quotes_on_values_that_start_with_an_at_sign() {
        // A bare value cannot start with `@` (SPEC 3.1), wherever it sits.
        let source = "VISIT /\nFILL Email \"@60s\"\nFILL Email \"@60s\" @5s\nFILL Email \"@60x\"\n";
        assert_eq!(fmt(source), source);
        // Behind a prefix, the `@` does not start the token.
        assert_eq!(
            fmt("VISIT /\nCLICK css:\"@x\"\n"),
            "VISIT /\nCLICK css:@x\n"
        );
    }

    #[test]
    fn keeps_quotes_on_keyword_shaped_values() {
        let source =
            "VISIT /\nCLICK \"Note: x\"\nCLICK \">>\"\nCLICK button:\"*\"\nPAGE \"matches\"\n";
        assert_eq!(fmt(source), source);
        // A role name is part of its token, so a keyword cannot end the
        // locator early (SPEC 6.1).
        assert_eq!(
            fmt("VISIT /\nASSERT button:\"visible\" visible\nCAPTURE x: link:\"text\" text\n"),
            "VISIT /\nASSERT button:visible visible\nCAPTURE x: link:text text\n"
        );
    }

    #[test]
    fn unquotes_role_names_without_keyword_conflicts() {
        assert_eq!(
            fmt("VISIT /\nCLICK button:\"Save\"\nASSERT alert:\"Saved\" visible\n"),
            "VISIT /\nCLICK button:Save\nASSERT alert:Saved visible\n"
        );
    }

    #[test]
    fn normalizes_blank_lines() {
        assert_eq!(
            fmt("\n[Options]\n\napp-url: https://x\n\n\nVISIT /\n\n\nPAGE /\n\n\n\nVISIT /b\n\n"),
            "[Options]\napp-url: https://x\n\nVISIT /\nPAGE /\n\nVISIT /b\n"
        );
    }

    #[test]
    fn preserves_comments_in_place() {
        assert_eq!(
            fmt(
                "# top\nVISIT /    # go\n# middle\nPAGE /\n\n\n# next entry\nVISIT /b\n# trailing\n"
            ),
            "# top\nVISIT / # go\n# middle\nPAGE /\n\n# next entry\nVISIT /b\n# trailing\n"
        );
    }

    #[test]
    fn keeps_comments_on_their_side_of_a_section_header() {
        assert_eq!(
            fmt("VISIT /\n# above\n# below\nASSERT url == /\n"),
            "VISIT /\n# above\n# below\nASSERT url == /\n"
        );
    }

    #[test]
    fn renders_escapes_canonically() {
        assert_eq!(
            fmt("VISIT /\nFILL A \"x\\u{9}y\"\nFILL B \"\\u{1F600}\"\nFILL C \"a\\{{b\"\n"),
            "VISIT /\nFILL A \"x\\ty\"\nFILL B \u{1F600}\nFILL C \"a\\{\\{b\"\n"
        );
    }
}
