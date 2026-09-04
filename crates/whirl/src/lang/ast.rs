//! Typed AST for parsed `.whirl` files (SPEC sections 3-10 and 17).
//!
//! The AST keeps enough source detail (spans, quoting, comment lines, raw
//! step text) for error rendering, report naming, and the later canonical
//! formatter. Values stay unresolved: interpolation segments are split at
//! parse time and resolved by the runner.

use std::path::PathBuf;

mod options;

/// Source position of a token or step: 1-based line, 1-based character
/// column, and length in characters (for caret rendering).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct Span {
    pub(crate) line:   u32,
    pub(crate) column: u32,
    pub(crate) len:    u32,
}

/// One piece of a value after interpolation splitting (SPEC 3.1, 11).
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum ValueSegment {
    /// Literal text with escapes already applied.
    Literal(String),
    /// `{{name}}` variable reference.
    Var(String),
    /// `{{env.NAME}}` environment variable reference.
    EnvVar(String),
    /// `{{setup.NAME}}`: a capture of the file's `setup` flow (SPEC 11).
    SetupVar(String),
}

/// A value: quoted or bare, split into interpolation segments.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct Value {
    pub(crate) segments: Vec<ValueSegment>,
    pub(crate) span:     Span,
    /// True when the source wrote the value in quotes. The formatter must
    /// not drop quotes whose removal would change the parse (SPEC 3.1).
    pub(crate) quoted:   bool,
}

impl Value {
    /// True when the value contains no variable references.
    pub(crate) fn is_literal(&self) -> bool {
        self.segments
            .iter()
            .all(|segment| matches!(segment, ValueSegment::Literal(_)))
    }

    /// The literal text of a value without variable references, if any.
    pub(crate) fn as_literal(&self) -> Option<String> {
        if !self.is_literal() {
            return None;
        }
        let mut text = String::new();
        for segment in &self.segments {
            if let ValueSegment::Literal(literal) = segment {
                text.push_str(literal);
            }
        }
        Some(text)
    }
}

/// A `/pattern/flags` regex literal (SPEC 3.1). The pattern is stored as
/// written (JavaScript syntax; the shim evaluates it).
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct Regex {
    pub(crate) pattern: String,
    pub(crate) flags:   RegexFlags,
    pub(crate) span:    Span,
}

/// Valid regex flags: `i`, `s`, `m`.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct RegexFlags {
    pub(crate) ignore_case: bool,
    pub(crate) dot_all:     bool,
    pub(crate) multiline:   bool,
}

/// A duration literal: non-negative integer plus `ms` or `s`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct DurationLit {
    pub(crate) amount: u64,
    pub(crate) unit:   DurationUnit,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum DurationUnit {
    Milliseconds,
    Seconds,
}

impl DurationLit {
    pub(crate) fn millis(self) -> u64 {
        match self.unit {
            DurationUnit::Milliseconds => self.amount,
            DurationUnit::Seconds => self.amount.saturating_mul(1000),
        }
    }
}

/// A locator: one or more segments joined by `>>` (SPEC 6).
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct Locator {
    pub(crate) segments: Vec<LocatorSegment>,
    pub(crate) span:     Span,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct LocatorSegment {
    pub(crate) kind: SegmentKind,
    pub(crate) span: Span,
}

/// Text-matching prefixes that share the optional `~` substring variant.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum TextPrefix {
    Label,
    Placeholder,
    Text,
    Alt,
    Title,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum SegmentKind {
    /// `role:TYPE` / `role~:TYPE` with an optional accessible name.
    Role {
        substring: bool,
        role:      String,
        name:      Option<Value>,
    },
    /// `label:` `placeholder:` `text:` `alt:` `title:` and their `~` forms.
    TextEngine {
        prefix:    TextPrefix,
        substring: bool,
        value:     Value,
    },
    /// `testid:id`.
    TestId(Value),
    /// `css:"selector"` — the escape hatch.
    Css(Value),
    /// `frame:"selector"` enters an iframe before the next element segment.
    Frame(Value),
    /// `nth:N`, 1-based. Never the first segment; N >= 1.
    Nth(u64),
    /// Unprefixed value; legal only in actions (SPEC 6.1). The default
    /// engine (`label:` or `text:`) depends on the action; see
    /// [`ActionKind::default_engine`].
    Default(Value),
}

/// The engine an unprefixed locator value selects (SPEC 6.1).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum DefaultEngine {
    Label,
    Text,
}

/// A comment in the source. `own_line` is true for full-line comments,
/// false for trailing comments after a step.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct Comment {
    pub(crate) line:     u32,
    pub(crate) column:   u32,
    /// Text after `#`, not trimmed.
    pub(crate) text:     String,
    pub(crate) own_line: bool,
}

/// An identifier (`[A-Za-z_][A-Za-z0-9_]*`) with its source span:
/// capture names and `SCREENSHOT`/`SNAPSHOT` names.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct Ident {
    pub(crate) text: String,
    pub(crate) span: Span,
}

/// Browser engines accepted by the `browser` option (SPEC 5).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum BrowserKind {
    Chromium,
    Firefox,
    Webkit,
}

/// Values of the `reduced-motion` option (SPEC 5): what the page's
/// `prefers-reduced-motion` media query reports.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ReducedMotion {
    Reduce,
    NoPreference,
}

/// Automatic dialog responses accepted by the `dialogs` option (SPEC 5).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum DialogPolicy {
    Dismiss,
    Accept,
}

/// A `WIDTHxHEIGHT` viewport size in CSS pixels (SPEC 3.1).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct Viewport {
    pub(crate) width:  u64,
    pub(crate) height: u64,
}

/// A typed option value. A literal value is shape-validated at parse time.
/// A value with `{{...}}` interpolation cannot be shape-checked until the
/// runner resolves it at file start (SPEC 11), so it stays a [`Value`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum OptionValue<T> {
    Literal(T),
    Interpolated(Value),
}

/// One `key: value` line in the `[Options]` section, typed per SPEC 5.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum FileOption {
    Base(Value),
    Browser(OptionValue<BrowserKind>),
    Viewport(OptionValue<Viewport>),
    StepTimeout(OptionValue<DurationLit>),
    EntryTimeout(OptionValue<DurationLit>),
    NavTimeout(OptionValue<DurationLit>),
    AllowHosts(Vec<Value>),
    Dialogs(OptionValue<DialogPolicy>),
    ReducedMotion(OptionValue<ReducedMotion>),
    Storage(Value),
    UserAgent(Value),
    /// `setup: path`, a flow whose final state this file starts from
    /// (SPEC 5, 12).
    Setup(Value),
}

impl File {
    /// The `setup:` option line, when the file has one.
    pub(crate) fn setup_option(&self) -> Option<&OptionLine> {
        self.options
            .iter()
            .find(|line| matches!(line.option, FileOption::Setup(_)))
    }

    /// The `storage:` option line, when the file has one.
    pub(crate) fn storage_option(&self) -> Option<&OptionLine> {
        self.options
            .iter()
            .find(|line| matches!(line.option, FileOption::Storage(_)))
    }
}

/// An `[Options]` line with its source position.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct OptionLine {
    pub(crate) option: FileOption,
    pub(crate) line:   u32,
    pub(crate) span:   Span,
}

/// An action line (SPEC 7).
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct Action {
    pub(crate) kind:    ActionKind,
    /// `@duration` step-timeout override (SPEC 12).
    pub(crate) timeout: Option<DurationLit>,
    pub(crate) line:    u32,
    pub(crate) span:    Span,
    /// The step's source text: the line without indentation, trailing
    /// comment, or trailing whitespace. Reports render this.
    pub(crate) text:    String,
}

/// The verb and operands of an action (SPEC 7, 17).
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum ActionKind {
    Http {
        name:    Ident,
        method:  String,
        url:     Value,
        headers: Vec<(String, Value)>,
        body:    Option<Value>,
    },
    Response {
        name:   Ident,
        method: String,
        url:    Value,
    },
    Popup {
        name: Ident,
    },
    Tab {
        name: Ident,
    },
    Close {
        name: Ident,
    },
    Visit {
        url: Value,
    },
    Click {
        target: Locator,
    },
    Dblclick {
        target: Locator,
    },
    Fill {
        target: Locator,
        value:  Value,
    },
    /// `TYPE locator "text"` sends one key event per character.
    Type {
        target: Locator,
        text:   Value,
    },
    /// `PRESS "Key"` has no target; `PRESS locator "Key"` has one.
    Press {
        target: Option<Locator>,
        key:    Value,
    },
    Check {
        target: Locator,
    },
    Uncheck {
        target: Locator,
    },
    Select {
        target: Locator,
        option: Value,
    },
    Hover {
        target: Locator,
    },
    /// The value is the path after the `file:` prefix.
    Upload {
        target: Locator,
        path:   Value,
    },
    Screenshot {
        name: Ident,
    },
    Snapshot {
        name: Ident,
    },
    Eval {
        script: Value,
    },
    /// `STORE local "key" "value"` writes one browser storage entry.
    Store {
        scope: StoreScope,
        key:   Value,
        value: Value,
    },
}

/// Browser storage a `STORE` action writes to (SPEC 7).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum StoreScope {
    Local,
    Session,
    Cookie,
}

impl StoreScope {
    pub(crate) fn keyword(self) -> &'static str {
        match self {
            Self::Local => "local",
            Self::Session => "session",
            Self::Cookie => "cookie",
        }
    }
}

impl ActionKind {
    /// The engine an unprefixed locator value selects in this action
    /// (SPEC 6.1), if the action targets elements.
    pub(crate) fn default_engine(&self) -> Option<DefaultEngine> {
        match self {
            Self::Fill { .. }
            | Self::Type { .. }
            | Self::Select { .. }
            | Self::Check { .. }
            | Self::Uncheck { .. }
            | Self::Upload { .. }
            | Self::Press { .. } => Some(DefaultEngine::Label),
            Self::Click { .. } | Self::Dblclick { .. } | Self::Hover { .. } => {
                Some(DefaultEngine::Text)
            }
            Self::Http { .. }
            | Self::Response { .. }
            | Self::Popup { .. }
            | Self::Tab { .. }
            | Self::Close { .. }
            | Self::Visit { .. }
            | Self::Screenshot { .. }
            | Self::Snapshot { .. }
            | Self::Eval { .. }
            | Self::Store { .. } => None,
        }
    }
}

/// A `PAGE` line (SPEC 8).
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct Page {
    pub(crate) check:   PageCheck,
    pub(crate) timeout: Option<DurationLit>,
    pub(crate) line:    u32,
    pub(crate) span:    Span,
    pub(crate) text:    String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum PageCheck {
    Value(Value),
    Matches(Regex),
}

/// One check line in an `[Asserts]` section (SPEC 9).
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct Assert {
    pub(crate) body:    AssertBody,
    pub(crate) timeout: Option<DurationLit>,
    pub(crate) line:    u32,
    pub(crate) span:    Span,
    pub(crate) text:    String,
}

/// The subject and check of an assert. `url` and `title` take string
/// checks only, so the shape is encoded per subject (SPEC 9.3, 17).
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum AssertBody {
    ResponseStatus {
        name:   Ident,
        op:     NumOp,
        status: u64,
    },
    ResponseValue {
        name:  Ident,
        field: ResponseField,
        check: StrCheck,
    },
    TabClosed {
        name: Ident,
    },
    ElementState {
        locator: Locator,
        state:   StateCheck,
    },
    ElementValue {
        locator: Locator,
        source:  ValueSource,
        check:   StrCheck,
    },
    ElementCount {
        locator: Locator,
        op:      NumOp,
        count:   u64,
    },
    Url(StrCheck),
    Title(StrCheck),
}

/// Element state checks (SPEC 9.1).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum StateCheck {
    Visible,
    Hidden,
    Enabled,
    Disabled,
    Checked,
    Unchecked,
    Focused,
}

/// What an element value check reads (SPEC 9.2).
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum ValueSource {
    Text,
    Value,
    /// `attr:NAME`; the name follows the `attr-name` production.
    Attr(String),
}

/// A string check: operator plus operand (SPEC 9.4).
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum StrCheck {
    Eq(Value),
    Ne(Value),
    Contains(Value),
    Matches(Regex),
}

/// Count comparison operators (SPEC 9.4).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum NumOp {
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
}

/// One line in a `[Captures]` section (SPEC 10).
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct Capture {
    pub(crate) name:    Ident,
    pub(crate) source:  CaptureSource,
    /// The optional `regex /re/` filter.
    pub(crate) filter:  Option<Regex>,
    pub(crate) timeout: Option<DurationLit>,
    pub(crate) line:    u32,
    pub(crate) span:    Span,
    pub(crate) text:    String,
}

/// Where a capture's value comes from (SPEC 10).
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum CaptureSource {
    Response {
        name:  Ident,
        field: ResponseField,
    },
    Element {
        locator:   Locator,
        extractor: Extractor,
    },
    Url,
    Title,
    Eval(Value),
}

/// A field from one named HTTP response.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum ResponseField {
    Status,
    Header(Value),
    Json(Value),
}

/// Element extractors for captures (SPEC 10).
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum Extractor {
    Text,
    Value,
    Count,
    Attr(String),
}

/// One entry: actions, then optional `PAGE`, `[Asserts]`, and
/// `[Captures]`, in that order (SPEC 4).
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct Entry {
    pub(crate) actions:         Vec<Action>,
    pub(crate) page:            Option<Page>,
    pub(crate) asserts:         Vec<Assert>,
    pub(crate) captures:        Vec<Capture>,
    /// The line of the entry's `[Asserts]` header, when the source has
    /// one (it may be present even with zero checks). The formatter uses
    /// it to keep comments on their side of the header.
    pub(crate) asserts_header:  Option<u32>,
    /// The line of the entry's `[Captures]` header, when present.
    pub(crate) captures_header: Option<u32>,
}

impl Entry {
    /// The entry's first action. An entry always has at least one action
    /// (SPEC 4), so parsed entries never hit the `expect`.
    fn first_action(&self) -> &Action {
        self.actions
            .first()
            .expect("a parsed entry always has at least one action")
    }

    /// The line of the entry's first action.
    pub(crate) fn line(&self) -> u32 {
        self.first_action().line
    }
}

/// A parsed `.whirl` file.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct File {
    pub(crate) path:           PathBuf,
    pub(crate) options:        Vec<OptionLine>,
    pub(crate) entries:        Vec<Entry>,
    /// Every comment in the file, in source order.
    pub(crate) comments:       Vec<Comment>,
    /// The line of the `[Options]` header, when the source has one (it
    /// may be present even with zero option lines).
    pub(crate) options_header: Option<u32>,
}

impl File {
    /// The display name of an entry for reports (SPEC 14): the text of the
    /// nearest own-line comment above the entry's first action with no
    /// other step between them (trimmed, without `#`), else the first
    /// action's source text plus its line number.
    pub(crate) fn entry_display_name(&self, entry: &Entry) -> String {
        let first = entry.first_action();
        let nearest_step_above = self
            .step_lines()
            .filter(|line| *line < first.line)
            .max()
            .unwrap_or(0);
        let comment = self
            .comments
            .iter()
            .filter(|comment| {
                comment.own_line && comment.line < first.line && comment.line > nearest_step_above
            })
            .max_by_key(|comment| comment.line);
        match comment {
            Some(comment) => comment.text.trim().to_owned(),
            None => format!("{} (line {})", first.text, first.line),
        }
    }

    /// The line numbers of every step and option line in the file.
    fn step_lines(&self) -> impl Iterator<Item = u32> {
        let option_lines = self.options.iter().map(|option| option.line);
        let entry_lines = self.entries.iter().flat_map(|entry| {
            let actions = entry.actions.iter().map(|action| action.line);
            let page = entry.page.iter().map(|page| page.line);
            let asserts = entry.asserts.iter().map(|assert| assert.line);
            let captures = entry.captures.iter().map(|capture| capture.line);
            actions.chain(page).chain(asserts).chain(captures)
        });
        option_lines.chain(entry_lines)
    }
}
