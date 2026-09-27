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
//! parse (SPEC 3.1): a value that would become a keyword, a
//! prefix-shaped segment, a `>>` separator, or a final `@duration`
//! timeout suffix stays quoted, as does any value with whitespace, `"`,
//! `#`, or characters that need escapes. Formatting is idempotent, and
//! re-parsing the output yields a structurally identical file.

use std::fmt::Write as _;

use crate::check::{Number, is_bytes_literal_shape};
use crate::lang::ast::{
    Action, ActionKind, Assert, AssertBody, Capture, CheckLine, Comment, DurationLit, DurationUnit,
    Entry, Extractor, File, FileOption, FilterArg, FilterSpec, HttpBodyKind, Locator, Operand,
    OptionValue, Page, PageCheck, PredicateSpec, Regex, ResponseField, SegmentKind, StateCheck,
    Subject, TextPrefix, Value, ValueSegment, Viewport,
};

/// Where a rendered value sits in its line. The context decides which
/// bare spellings would change the parse and therefore need quotes.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ValueCtx {
    /// A standalone value with no keyword conflicts: URLs, fill text,
    /// keys, scripts, check operands, and option values.
    Plain,
    /// The `PAGE` value; a bare `matches` would start a regex check.
    Page,
    /// An unprefixed default-engine segment in an action locator; a
    /// prefix-shaped or `>>` spelling would change the segment kind.
    ActionDefault,
    /// A role's accessible name in an action locator.
    ActionRoleName,
    /// An unprefixed default-engine segment in a `DRAG` locator, where a
    /// bare `to` also separates the two locators.
    DragDefault,
    /// A role's accessible name in a `DRAG` locator.
    DragRoleName,
    /// A role's accessible name in an `[Asserts]` locator; a bare check
    /// keyword would end the locator instead (SPEC 3.1).
    AssertRoleName,
    /// A role's accessible name in a `[Captures]` locator; a bare
    /// extractor keyword would end the locator instead.
    CaptureRoleName,
    /// A value attached to a segment or `file:` prefix; the prefix
    /// shields it from keyword and timeout readings.
    Prefixed,
    /// A check's expected value; a bare `[` would start a JSON literal.
    Operand,
}

/// Every locator-segment prefix spelling (SPEC 6.1).
const PREFIX_MARKERS: [&str; 16] = [
    "role:",
    "role~:",
    "label:",
    "label~:",
    "placeholder:",
    "placeholder~:",
    "text:",
    "text~:",
    "alt:",
    "alt~:",
    "title:",
    "title~:",
    "testid:",
    "css:",
    "frame:",
    "nth:",
];

const STATE_KEYWORDS: [&str; 7] = [
    "visible",
    "hidden",
    "enabled",
    "disabled",
    "checked",
    "unchecked",
    "focused",
];

fn is_assert_stop(text: &str) -> bool {
    STATE_KEYWORDS.contains(&text)
        || matches!(text, "text" | "value" | "count")
        || text.starts_with("attr:")
}

fn is_extractor_stop(text: &str) -> bool {
    matches!(text, "text" | "value" | "count") || text.starts_with("attr:")
}

fn is_prefix_shaped(text: &str) -> bool {
    PREFIX_MARKERS.iter().any(|marker| text.starts_with(marker))
}

/// True when a character can sit in a bare token without changing the
/// parse. Whitespace, `"`, and `#` end a bare token; `\` and `{` take
/// part in escapes and interpolation; control characters need `\u{...}`
/// escapes, which only quoted values have.
fn bare_safe_char(ch: char) -> bool {
    !ch.is_whitespace() && !ch.is_control() && !matches!(ch, '"' | '#' | '\\' | '{')
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
fn bare_changes_parse(text: &str, ctx: ValueCtx, is_final: bool) -> bool {
    if is_final
        && ctx != ValueCtx::Prefixed
        && text
            .strip_prefix('@')
            .is_some_and(|rest| rest.parse::<DurationLit>().is_ok())
    {
        // A final bare token of the form `@duration` is the timeout
        // suffix (SPEC 3.1); any other `@...` token is an ordinary value.
        return true;
    }
    match ctx {
        ValueCtx::Plain | ValueCtx::Prefixed => false,
        ValueCtx::Operand => text.starts_with('['),
        ValueCtx::Page => text == "matches",
        ValueCtx::ActionDefault => is_prefix_shaped(text) || text == ">>",
        ValueCtx::ActionRoleName => text == ">>",
        ValueCtx::DragDefault => is_prefix_shaped(text) || text == ">>" || text == "to",
        ValueCtx::DragRoleName => text == ">>" || text == "to",
        ValueCtx::AssertRoleName => text == ">>" || is_assert_stop(text),
        ValueCtx::CaptureRoleName => text == ">>" || is_extractor_stop(text),
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
/// context, quoted otherwise. `is_final` marks the line's last token,
/// where a bare `@...` spelling would become the timeout suffix.
fn render_value(value: &Value, ctx: ValueCtx, is_final: bool) -> String {
    match bare_candidate(value) {
        Some(text) if !bare_changes_parse(&text, ctx, is_final) => text,
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

/// Which grammar position a locator sits in; it picks the context for
/// role accessible names.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum LocatorCtx {
    Action,
    /// A `DRAG` locator, where a bare `to` is the separator.
    Drag,
    Assert,
    Capture,
}

impl LocatorCtx {
    fn role_name_ctx(self) -> ValueCtx {
        match self {
            Self::Action => ValueCtx::ActionRoleName,
            Self::Drag => ValueCtx::DragRoleName,
            Self::Assert => ValueCtx::AssertRoleName,
            Self::Capture => ValueCtx::CaptureRoleName,
        }
    }

    fn default_ctx(self) -> ValueCtx {
        match self {
            Self::Drag => ValueCtx::DragDefault,
            Self::Action | Self::Assert | Self::Capture => ValueCtx::ActionDefault,
        }
    }
}

/// Renders one locator segment as its token(s).
fn render_segment(kind: &SegmentKind, ctx: LocatorCtx, is_final: bool) -> String {
    match kind {
        SegmentKind::Role {
            substring,
            role,
            name,
        } => {
            let marker = if *substring { "role~:" } else { "role:" };
            match name {
                Some(name) => format!(
                    "{marker}{role} {}",
                    render_value(name, ctx.role_name_ctx(), is_final)
                ),
                None => format!("{marker}{role}"),
            }
        }
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
            let marker = if *substring { "~:" } else { ":" };
            format!(
                "{name}{marker}{}",
                render_value(value, ValueCtx::Prefixed, false)
            )
        }
        SegmentKind::TestId(value) => {
            format!("testid:{}", render_value(value, ValueCtx::Prefixed, false))
        }
        SegmentKind::Css(value) => {
            format!("css:{}", render_value(value, ValueCtx::Prefixed, false))
        }
        SegmentKind::Frame(value) => {
            format!("frame:{}", render_value(value, ValueCtx::Prefixed, false))
        }
        SegmentKind::Nth(index) => format!("nth:{index}"),
        SegmentKind::Default(value) => render_value(value, ctx.default_ctx(), is_final),
    }
}

/// Renders a locator: segments joined by ` >> `. `is_final` marks a
/// locator that ends its line, where the last token must not read as a
/// timeout suffix.
fn render_locator(locator: &Locator, ctx: LocatorCtx, is_final: bool) -> String {
    let last = locator.segments.len().saturating_sub(1);
    locator
        .segments
        .iter()
        .enumerate()
        .map(|(index, segment)| render_segment(&segment.kind, ctx, is_final && index == last))
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
fn render_action(action: &Action) -> String {
    let is_final = action.timeout.is_none();
    let mut out = match &action.kind {
        ActionKind::Http { method, url, .. } => {
            format!(
                "HTTP {method} {}",
                render_value(url, ValueCtx::Plain, is_final)
            )
        }
        ActionKind::Response { name, method, url } => format!(
            "RESPONSE {} {method} {}",
            name.text,
            render_value(url, ValueCtx::Plain, is_final)
        ),
        ActionKind::Popup { name } => format!("POPUP {}", name.text),
        ActionKind::Tab { name } => format!("TAB {}", name.text),
        ActionKind::Close { name } => format!("CLOSE {}", name.text),
        ActionKind::Visit { url } => {
            format!("VISIT {}", render_value(url, ValueCtx::Plain, is_final))
        }
        ActionKind::Click { target, button } => format!(
            "{} {}",
            button.keyword(),
            render_locator(target, LocatorCtx::Action, is_final)
        ),
        ActionKind::Dblclick { target } => format!(
            "DBLCLICK {}",
            render_locator(target, LocatorCtx::Action, is_final)
        ),
        ActionKind::Fill { target, value } => format!(
            "FILL {} {}",
            render_locator(target, LocatorCtx::Action, false),
            render_value(value, ValueCtx::Plain, is_final)
        ),
        ActionKind::Type { target, text } => format!(
            "TYPE {} {}",
            render_locator(target, LocatorCtx::Action, false),
            render_value(text, ValueCtx::Plain, is_final)
        ),
        ActionKind::Press { target: None, key } => {
            format!("PRESS {}", render_value(key, ValueCtx::Plain, is_final))
        }
        ActionKind::Press {
            target: Some(target),
            key,
        } => format!(
            "PRESS {} {}",
            render_locator(target, LocatorCtx::Action, false),
            render_value(key, ValueCtx::Plain, is_final)
        ),
        ActionKind::Check { target } => {
            format!(
                "CHECK {}",
                render_locator(target, LocatorCtx::Action, is_final)
            )
        }
        ActionKind::Uncheck { target } => format!(
            "UNCHECK {}",
            render_locator(target, LocatorCtx::Action, is_final)
        ),
        ActionKind::Select { target, option } => format!(
            "SELECT {} {}",
            render_locator(target, LocatorCtx::Action, false),
            render_value(option, ValueCtx::Plain, is_final)
        ),
        ActionKind::Hover { target } => {
            format!(
                "HOVER {}",
                render_locator(target, LocatorCtx::Action, is_final)
            )
        }
        ActionKind::Drag { source, target } => format!(
            "DRAG {} to {}",
            render_locator(source, LocatorCtx::Drag, false),
            render_locator(target, LocatorCtx::Drag, is_final)
        ),
        ActionKind::Upload { target, path } => format!(
            "UPLOAD {} file:{}",
            render_locator(target, LocatorCtx::Action, false),
            render_value(path, ValueCtx::Prefixed, false)
        ),
        ActionKind::Screenshot { name } => format!("SCREENSHOT {}", name.text),
        ActionKind::Snapshot { name } => format!("SNAPSHOT {}", name.text),
        ActionKind::Eval { script } => {
            format!("EVAL {}", render_value(script, ValueCtx::Plain, is_final))
        }
        ActionKind::Act { scope, instruction } => match scope {
            Some(scope) => format!(
                "ACT {} {}",
                render_locator(scope, LocatorCtx::Action, false),
                render_value(instruction, ValueCtx::Plain, is_final)
            ),
            None => format!(
                "ACT {}",
                render_value(instruction, ValueCtx::Plain, is_final)
            ),
        },
        ActionKind::Store { scope, key, value } => format!(
            "STORE {} {} {}",
            scope.keyword(),
            render_value(key, ValueCtx::Plain, false),
            render_value(value, ValueCtx::Plain, is_final)
        ),
    };
    push_timeout(&mut out, action.timeout);
    out
}

/// Renders a `PAGE` line (SPEC 8).
fn render_page(page: &Page) -> String {
    let is_final = page.timeout.is_none();
    let mut out = match &page.check {
        PageCheck::Value(value) => {
            format!("PAGE {}", render_value(value, ValueCtx::Page, is_final))
        }
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
fn render_operand(operand: &Operand, is_final: bool) -> String {
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
                render_value(value, ValueCtx::Operand, is_final)
            }
        }
    }
}

/// Renders `[not] predicate` (SPEC 9.4).
fn render_predicate(negated: bool, predicate: &PredicateSpec, is_final: bool) -> String {
    let body = match predicate {
        PredicateSpec::Compare { kind, expected } => {
            format!("{} {}", kind.name(), render_operand(expected, is_final))
        }
        PredicateSpec::Matches(regex) => format!("matches {}", render_regex(regex)),
        PredicateSpec::Word(kind) => kind.name().to_owned(),
    };
    if negated { format!("not {body}") } else { body }
}

/// Renders one filter; `is_final` marks the line's last token.
fn render_filter(filter: &FilterSpec, is_final: bool) -> String {
    let mut out = filter.kind.name().to_owned();
    let last = filter.args.len().saturating_sub(1);
    for (index, arg) in filter.args.iter().enumerate() {
        match arg {
            FilterArg::Value(value) if out.ends_with(':') => {
                out.push_str(&render_value(value, ValueCtx::Prefixed, false));
            }
            FilterArg::Value(value) => {
                out.push(' ');
                out.push_str(&render_value(
                    value,
                    ValueCtx::Plain,
                    is_final && index == last,
                ));
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

/// Renders a subject (SPEC 9.2); `is_final` marks it as the line's last
/// token, which only an `eval` script can be.
fn render_subject(subject: &Subject, ctx: LocatorCtx, is_final: bool) -> String {
    match subject {
        Subject::Element { locator, extractor } => format!(
            "{} {}",
            render_locator(locator, ctx, false),
            render_extractor(extractor)
        ),
        Subject::Url => "url".to_owned(),
        Subject::Title => "title".to_owned(),
        Subject::Eval(script) => {
            format!("eval {}", render_value(script, ValueCtx::Plain, is_final))
        }
        Subject::Response { name: None, field } => render_response_field(field),
        Subject::Response {
            name: Some(name),
            field,
        } => format!("response:{} {}", name.text, render_response_field(field)),
    }
}

/// Renders `subject { filter }`; `is_final` marks the last token.
fn render_chain(
    subject: &Subject,
    filters: &[FilterSpec],
    ctx: LocatorCtx,
    is_final: bool,
) -> String {
    let mut out = render_subject(subject, ctx, is_final && filters.is_empty());
    let last = filters.len().saturating_sub(1);
    for (index, filter) in filters.iter().enumerate() {
        out.push(' ');
        out.push_str(&render_filter(filter, is_final && index == last));
    }
    out
}

fn render_check(check: &CheckLine, is_final: bool) -> String {
    format!(
        "{} {}",
        render_chain(&check.subject, &check.filters, LocatorCtx::Assert, false),
        render_predicate(check.negated, &check.predicate, is_final)
    )
}

/// Renders an `[Asserts]` line (SPEC 9).
fn render_assert(assert: &Assert) -> String {
    let is_final = assert.timeout.is_none();
    let mut out = match &assert.body {
        AssertBody::TabClosed { name } => format!("tab:{} closed", name.text),
        AssertBody::ElementState { locator, state } => format!(
            "{} {}",
            render_locator(locator, LocatorCtx::Assert, false),
            state_check_text(*state)
        ),
        AssertBody::Check(check) => render_check(check, is_final),
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
            format!("header:{}", render_value(value, ValueCtx::Prefixed, false))
        }
        ResponseField::Json(value) => {
            format!("json:{}", render_value(value, ValueCtx::Prefixed, false))
        }
        ResponseField::Xpath(value) => {
            format!("xpath:{}", render_value(value, ValueCtx::Prefixed, false))
        }
    }
}

/// Renders a `[Captures]` line (SPEC 10).
fn render_capture(capture: &Capture) -> String {
    let is_final = capture.timeout.is_none();
    let chain = render_chain(
        &capture.subject,
        &capture.filters,
        LocatorCtx::Capture,
        is_final,
    );
    let mut out = format!("{}: {chain}", capture.name.text);
    push_timeout(&mut out, capture.timeout);
    out
}

fn render_option_value<T>(value: &OptionValue<T>, literal: impl Fn(&T) -> String) -> String {
    match value {
        OptionValue::Literal(typed) => literal(typed),
        OptionValue::Interpolated(value) => render_value(value, ValueCtx::Plain, false),
    }
}

fn viewport_text(viewport: Viewport) -> String {
    format!("{}x{}", viewport.width, viewport.height)
}

/// Renders one `[Options]` line (SPEC 5).
fn render_option(option: &FileOption) -> String {
    let plain = |value: &Value| render_value(value, ValueCtx::Plain, false);
    match option {
        FileOption::Base(value) => format!("base: {}", plain(value)),
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
        FileOption::AllowHosts(globs) => {
            let globs: Vec<String> = globs.iter().map(plain).collect();
            format!("allow-hosts: {}", globs.join(" "))
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
        if let ActionKind::Http { headers, body, .. } = &action.kind {
            for header in headers {
                lines.push(Line {
                    source_line: header.line,
                    text:        format!(
                        "{}: {}",
                        header.name,
                        render_value(&header.value, ValueCtx::Plain, false)
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
    if let Some(header) = entry.asserts_header {
        lines.push(Line {
            source_line: header,
            text:        "[Asserts]".to_owned(),
        });
    }
    for assert in &entry.asserts {
        lines.push(Line {
            source_line: assert.line,
            text:        render_assert(assert),
        });
    }
    if let Some(header) = entry.captures_header {
        lines.push(Line {
            source_line: header,
            text:        "[Captures]".to_owned(),
        });
    }
    for capture in &entry.captures {
        lines.push(Line {
            source_line: capture.line,
            text:        render_capture(capture),
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
pub(crate) fn format_file(file: &File) -> String {
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
    use crate::lang::ast::{Ident, LocatorSegment, OptionLine, Span};
    use crate::lang::parse::parse_file;

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
            | SegmentKind::Default(value) => scrub_value(value),
            SegmentKind::Nth(_) => {}
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

    fn scrub_option_line(option: &mut OptionLine) {
        option.line = 0;
        option.span = ZERO;
        match &mut option.option {
            FileOption::Base(value)
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
            FileOption::AllowHosts(globs) => {
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
            ActionKind::Click { target, .. }
            | ActionKind::Dblclick { target }
            | ActionKind::Check { target }
            | ActionKind::Uncheck { target }
            | ActionKind::Hover { target } => scrub_locator(target),
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
            } => {
                scrub_locator(target);
                scrub_value(value);
            }
            ActionKind::Drag { source, target } => {
                scrub_locator(source);
                scrub_locator(target);
            }
            ActionKind::Press { target, key } => {
                if let Some(target) = target {
                    scrub_locator(target);
                }
                scrub_value(key);
            }
            ActionKind::Popup { name }
            | ActionKind::Tab { name }
            | ActionKind::Close { name }
            | ActionKind::Screenshot { name }
            | ActionKind::Snapshot { name } => scrub_ident(name),
            ActionKind::Eval { script } => scrub_value(script),
            ActionKind::Act { scope, instruction } => {
                if let Some(scope) = scope {
                    scrub_locator(scope);
                }
                scrub_value(instruction);
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
        entry.asserts_header = entry.asserts_header.map(|_| 0);
        entry.captures_header = entry.captures_header.map(|_| 0);
        for assert in &mut entry.asserts {
            assert.line = 0;
            assert.span = ZERO;
            assert.text = String::new();
            match &mut assert.body {
                AssertBody::TabClosed { name } => scrub_ident(name),
                AssertBody::ElementState { locator, .. } => scrub_locator(locator),
                AssertBody::Check(check) => {
                    scrub_subject(&mut check.subject);
                    scrub_filters(&mut check.filters);
                    scrub_predicate(&mut check.predicate);
                }
            }
        }
        for capture in &mut entry.captures {
            capture.line = 0;
            capture.span = ZERO;
            capture.text = String::new();
            scrub_ident(&mut capture.name);
            scrub_subject(&mut capture.subject);
            scrub_filters(&mut capture.filters);
        }
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
        assert_round_trip("VISIT /\nACT css:form >> role:group \"click Buy\"\n");
        assert_eq!(
            fmt("VISIT /\nACT   css:form   \"click Buy\"\n"),
            "VISIT /\nACT css:form \"click Buy\"\n"
        );
    }

    // ---- Fixtures --------------------------------------------------

    /// Valid sources covering every construct; each must round-trip.
    const FIXTURES: [&str; 21] = [
        // The SPEC section 2 example.
        "# checkout.whirl \u{2014} buy a widget as a signed-in user.\n[Options]\nbase: https://shop.example.com\nviewport: 1280x800\n\n# Log in.\nVISIT /login\n\nFILL \"Email\" alice@example.com\nFILL \"Password\" {{env.TEST_PASSWORD}}\nCLICK role:button \"Sign in\"\nPAGE /dashboard\n[Asserts]\nrole:heading \"Welcome back\" visible\ntestid:user-menu text == Alice\n\n# Find a product.\nFILL placeholder:\"Search products\" widget\nPRESS Enter\n[Asserts]\nurl contains \"q=widget\"\ntestid:result-card count >= 1\n[Captures]\nfirst_product: testid:result-card >> nth:1 >> role:link attr:href\n\n# Add it to the cart.\nVISIT {{first_product}}\nCLICK \"Add to cart\"\n[Asserts]\ntestid:cart-badge text == 1\nrole:alert text contains \"Added to cart\"\n",
        // Every option key, including interpolated values.
        "[Options]\nbase: https://example.com\nbrowser: webkit\nviewport: 800x600\nstep-timeout: 5s\nentry-timeout: 90s\nnav-timeout: 45s\nallow-hosts: example.com *.example.com\ndialogs: accept\nreduced-motion: reduce\nstorage: auth/state.json\nuser-agent: \"Mozilla/5.0 (Whirl)\"\nsetup: sign-in.whirl\nVISIT /\n",
        "[Options]\nbrowser: {{engine}}\nviewport: {{size}}\nstep-timeout: {{t}}\nVISIT /\n",
        // Every action form.
        "VISIT /a\nCLICK \"Add to cart\"\nRIGHTCLICK \"report.pdf\"\nMIDDLECLICK role:link Docs\nDBLCLICK text~:\"added\"\nFILL \"Email\" alice@example.com\nTYPE \"Code\" 424242\nPRESS Enter\nPRESS label:Search \"Control+A\"\nCHECK \"Remember me\"\nUNCHECK role:checkbox \"Spam\"\nSELECT \"Country\" \"United States\"\nHOVER testid:menu\nDRAG \"Write spec\" to testid:done\nDRAG \"to\" to role:listitem \"to\"\nUPLOAD \"Avatar\" file:images/cat.png\nSCREENSHOT overview\nSNAPSHOT header\nEVAL \"window.scrollTo(0, 0)\"\nSTORE local onboarding:done yes\nSTORE local \"welcome seen\" {{env.SEEN}}\nSTORE session draft hi\nSTORE cookie chat_version v1\nVISIT /u/{{setup.user_id}}\n",
        // Timeout suffixes on every step kind.
        "VISIT / @45s\nCLICK go @60s\nPAGE /done @2s\n[Asserts]\ntestid:x visible @2500ms\nurl == / @1s\n[Captures]\nn: testid:x text @3s\nm: testid:x text regex /x(y)?/ @3s\n",
        // Every assert form and operator.
        "VISIT /\n[Asserts]\ntestid:a visible\ntestid:a hidden\ntestid:a enabled\ntestid:a disabled\ntestid:a checked\ntestid:a unchecked\ntestid:a focused\ntestid:a text == x\ntestid:a text != x\ntestid:a text contains x\ntestid:a text matches /Order #\\w+/i\ntestid:a value == 0\ntestid:a attr:aria-expanded == true\ntestid:a attr:data-state != open\ntestid:a count == 3\ntestid:a count != 3\ntestid:a count < 3\ntestid:a count <= 3\ntestid:a count > 3\ntestid:a count >= 3\nurl == https://x/\nurl matches /a.b/ism\ntitle contains Check\n",
        // Every capture form.
        "VISIT /\n[Captures]\na: testid:x text\nb: label:Amount value\nc: css:\".row\" count\nd: role:link \"Docs\" attr:href\ne: url\nf: title\ng: eval \"document.title\"\nh: testid:x text regex /Order #(\\w+)/\n",
        // Locator shapes: chains, nth, every prefix and substring form.
        "VISIT /\nCLICK role:button \"Sign in\"\nCLICK role~:button \"sign\"\nCLICK label:Email >> nth:2\nCLICK label~:mail\nCLICK placeholder:Search\nCLICK placeholder~:sea\nCLICK text:Go\nCLICK text~:go\nCLICK alt:Logo\nCLICK alt~:logo\nCLICK title:Info\nCLICK title~:info\nCLICK testid:cart >> css:\".x > .y\" >> nth:1\n",
        // Quotes the parse depends on.
        "VISIT /\nCLICK \"css:foo\"\nCLICK \"role:button\"\nCLICK \"nth:2\"\nCLICK \">>\"\nFILL Email \"@60s\"\nFILL Email \"@60x\"\nPAGE \"matches\"\n[Asserts]\nrole:button \"visible\" visible\nrole:button \"count\" text == \"@5s\"\n[Captures]\nx: role:link \"text\" text\n",
        // Values that are safe to bare.
        "VISIT \"/dashboard\"\nFILL \"Email\" \"alice\"\nPAGE \"/x\"\n[Asserts]\nurl == \"q\"\n",
        // Escapes and interpolation.
        "VISIT /\nFILL \"Says \\\"hi\\\"\" \"a\\tb\\nc\\\\d\"\nFILL \"U\" \"\\u{1F600}ok\"\nFILL \"B\" \"\\{{literal\"\nVISIT a\\{{b\nVISIT {{base_url}}/next\nFILL \"P\" {{env.SECRET}}\n",
        // Comments everywhere.
        "# top\n[Options] # inline options\nbase: https://x # inline base\n\n# name entry one\nVISIT / # go\n# between actions\nCLICK x\nPAGE / # landed\n[Asserts] # checks\n# before check\nurl == / # eq\n[Captures] # caps\n# before cap\nc: url # cap\n\n# name entry two\nVISIT /two\n# trailing comment\n",
        // Blank-line and spacing noise.
        "\n\n[Options]\n\n\nbase:    https://x\n\n\nVISIT     /\n\n\nPAGE      /\n\n\n\nVISIT   /b\n\n",
        // CRLF line endings.
        "VISIT /\r\nPAGE /\r\n[Asserts]\r\nurl == /\r\n",
        // Empty sections keep their headers.
        "VISIT /\n[Asserts]\n",
        "VISIT /\n[Asserts]\n[Captures]\n",
        "[Options]\nVISIT /\n",
        // PRESS one-argument vs two-argument forms.
        "VISIT /\nPRESS Enter\nPRESS \"Control+A\"\nPRESS label:Search Enter\nPRESS role:textbox \"Query\" Enter\n",
        // Default-engine values that stay bare.
        "VISIT /\nCLICK Save\nFILL Email alice\nUPLOAD Avatar file:cat.png\nSELECT Country France\n",
        // Attached prefix values that need quotes.
        "VISIT /\nCLICK css:\".a .b\" >> text:\"Add to cart\"\nCLICK label:\"First name\"\nUPLOAD \"Avatar\" file:\"my cat.png\"\n",
        // Capture names and eval edge spellings.
        "VISIT /\n[Captures]\na_1: eval \"1 + 1\"\nb: eval regex\nc: eval \"@5s\"\nd: eval x regex /y/\n",
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
[Asserts]
status == 201
HTTP GET /
X-Value: @10s
HTTP GET "@10s"
"#,
        );
    }

    #[test]
    fn collapses_spacing_to_single_spaces() {
        assert_eq!(
            fmt("VISIT    /login\nCLICK   role:button    \"Sign in\"   @5s\n"),
            "VISIT /login\nCLICK role:button \"Sign in\" @5s\n"
        );
    }

    #[test]
    fn removes_quotes_a_bare_spelling_preserves() {
        assert_eq!(
            fmt("VISIT \"/dashboard\"\nFILL \"Email\" \"alice\"\n[Asserts]\nurl == \"q\"\n"),
            "VISIT /dashboard\nFILL Email alice\n[Asserts]\nurl == q\n"
        );
    }

    #[test]
    fn keeps_quotes_on_typed_literal_spellings() {
        assert_eq!(
            fmt(
                "VISIT /\n[Asserts]\ntestid:x text == \"1\"\nurl == \"true\"\nurl != \"paid\"\neval \"1\" == 1\neval \"1\" == \"[a]\"\n"
            ),
            "VISIT /\n[Asserts]\ntestid:x text == \"1\"\nurl == \"true\"\nurl != paid\neval 1 == 1\neval 1 == \"[a]\"\n"
        );
    }

    #[test]
    fn keeps_quotes_on_interpolated_expected_values() {
        let source = "HTTP GET /x\n[Asserts]\nstatus == 200\njson:$.id == \"{{order_id}}\"\njson:$.n == \"{{a}}1\"\njson:$.id == {{order_id}}\n";
        assert_eq!(fmt(source), source);
    }

    #[test]
    fn keeps_json_literals_as_written() {
        let source =
            "VISIT /\nRESPONSE r GET /x\n[Asserts]\nresponse:r json:$.a == {\"b\":  [1,2]}\n";
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
    fn drag_keeps_quotes_on_a_to_that_is_text() {
        assert_eq!(
            fmt(
                "VISIT /\nDRAG \"Card\" to \"Done\"\nDRAG \"to\" to role:region \"to\"\nCLICK \"to\"\n"
            ),
            "VISIT /\nDRAG Card to Done\nDRAG \"to\" to role:region \"to\"\nCLICK to\n"
        );
    }

    #[test]
    fn responses_round_trip_with_fields_captures_and_interpolation() {
        assert_round_trip(
            r#"VISIT /
CLICK Order
RESPONSE order POST /api/orders @30s
[Asserts]
response:order status >= 200
response:order header:Content-Type contains application/json
response:order json:"$['key with space']" == true
response:order json:{{path}} matches /paid/i
response:order json:$.total toFloat >= 1e3
response:order json:$.tags == ["a", "{{tag}}"]
response:order json:$.id == "42"
response:order json:$.items[*].sku not contains ABC-1
response:order bytes startsWith hex,7b;
response:order location urlQueryParam next == /cart
response:order xpath:"count(//li[@class='row'])" == 3
response:order xpath://_:entry count >= 1
response:order body xpath:"string(//h1)" == Checks
response:order xpath:{{expr}} exists
[Captures]
body: response:order json:$
id: response:order json:$.id toString regex /order-(\d+)/ @2s
status: response:order status
header: response:order header:{{header_name}}
"#,
        );
    }

    #[test]
    fn named_tabs_round_trip() {
        assert_round_trip(
            "VISIT /\nCLICK Pay\nPOPUP payment @30s\nTAB payment\nCLOSE payment\n[Asserts]\ntab:payment closed @5s\nTAB main\n",
        );
    }

    #[test]
    fn frame_locators_round_trip_in_actions_asserts_and_captures() {
        assert_round_trip(
            r##"VISIT /
CLICK "frame:literal"
FILL frame:"#payment iframe" >> nth:2 >> frame:iframe >> label:Email alice@example.com
[Asserts]
frame:"#payment iframe" >> label:Email value == alice@example.com
[Captures]
email: frame:"#payment iframe" >> label:Email value
"##,
        );
    }

    #[test]
    fn keeps_quotes_on_prefix_shaped_default_segments() {
        let source = "VISIT /\nCLICK \"css:foo\"\nCLICK \"role:button\"\nCLICK \"nth:2\"\n";
        assert_eq!(fmt(source), source);
    }

    #[test]
    fn keeps_quotes_on_final_duration_shaped_values() {
        let source = "VISIT /\nFILL Email \"@60s\"\n";
        assert_eq!(fmt(source), source);
        // A timeout suffix takes the final position, so the value can
        // go bare.
        assert_eq!(
            fmt("VISIT /\nFILL Email \"@60s\" @5s\n"),
            "VISIT /\nFILL Email @60s @5s\n"
        );
        // A final `@` token that is not a valid duration is an ordinary
        // value (SPEC 3.1), so its quotes are not required.
        assert_eq!(
            fmt("VISIT /\nFILL Email \"@60x\"\n"),
            "VISIT /\nFILL Email @60x\n"
        );
    }

    #[test]
    fn keeps_quotes_on_keyword_shaped_values() {
        let source = "VISIT /\nPAGE \"matches\"\n[Asserts]\nrole:button \"visible\" visible\n[Captures]\nx: role:link \"text\" text\n";
        assert_eq!(fmt(source), source);
    }

    #[test]
    fn unquotes_role_names_without_keyword_conflicts() {
        assert_eq!(
            fmt("VISIT /\nCLICK role:button \"Save\"\n[Asserts]\nrole:alert \"Saved\" visible\n"),
            "VISIT /\nCLICK role:button Save\n[Asserts]\nrole:alert Saved visible\n"
        );
    }

    #[test]
    fn normalizes_blank_lines() {
        assert_eq!(
            fmt("\n[Options]\n\nbase: https://x\n\n\nVISIT /\n\n\nPAGE /\n\n\n\nVISIT /b\n\n"),
            "[Options]\nbase: https://x\n\nVISIT /\nPAGE /\n\nVISIT /b\n"
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
            fmt("VISIT /\n# above\n[Asserts]\n# below\nurl == /\n"),
            "VISIT /\n# above\n[Asserts]\n# below\nurl == /\n"
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
