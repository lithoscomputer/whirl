//! Typed AST for parsed `.whirl` files (SPEC sections 3-10 and 17).
//!
//! The AST keeps enough source detail (spans, quoting, comment lines, raw
//! step text) for error rendering, report naming, and the later canonical
//! formatter. Values stay unresolved: interpolation segments are split at
//! parse time and resolved by the runner.

use std::path::PathBuf;

/// Source position of a token or step: 1-based line, 1-based character
/// column, and length in characters (for caret rendering).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Span {
    pub line:   u32,
    pub column: u32,
    pub len:    u32,
}

/// One piece of a value after interpolation splitting (SPEC 3.1, 11).
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ValueSegment {
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
pub struct Value {
    pub segments: Vec<ValueSegment>,
    pub span:     Span,
    /// True when the source wrote the value in quotes. The formatter must
    /// not drop quotes whose removal would change the parse (SPEC 3.1).
    pub quoted:   bool,
}

impl Value {
    /// True when the value contains no variable references.
    pub fn is_literal(&self) -> bool {
        self.segments
            .iter()
            .all(|segment| matches!(segment, ValueSegment::Literal(_)))
    }

    /// The literal text of a value without variable references, if any.
    pub fn as_literal(&self) -> Option<String> {
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
pub struct Regex {
    pub pattern: String,
    pub flags:   RegexFlags,
    pub span:    Span,
}

/// Valid regex flags: `i`, `s`, `m`.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct RegexFlags {
    pub ignore_case: bool,
    pub dot_all:     bool,
    pub multiline:   bool,
}

/// A duration literal: non-negative integer plus `ms` or `s`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DurationLit {
    pub amount: u64,
    pub unit:   DurationUnit,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DurationUnit {
    Milliseconds,
    Seconds,
}

impl DurationLit {
    pub fn millis(self) -> u64 {
        match self.unit {
            DurationUnit::Milliseconds => self.amount,
            DurationUnit::Seconds => self.amount.saturating_mul(1000),
        }
    }
}

/// A locator: one or more segments joined by `>>` (SPEC 6).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Locator {
    pub segments: Vec<LocatorSegment>,
    pub span:     Span,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LocatorSegment {
    pub kind: SegmentKind,
    pub span: Span,
}

/// Text-matching prefixes that share the optional `~` substring variant.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TextPrefix {
    Label,
    Placeholder,
    Text,
    Alt,
    Title,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SegmentKind {
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
pub enum DefaultEngine {
    Label,
    Text,
}

/// A comment in the source. `own_line` is true for full-line comments,
/// false for trailing comments after a step.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Comment {
    pub line:     u32,
    pub column:   u32,
    /// Text after `#`, not trimmed.
    pub text:     String,
    pub own_line: bool,
}

/// An identifier (`[A-Za-z_][A-Za-z0-9_]*`) with its source span:
/// capture names and `SCREENSHOT`/`SNAPSHOT` names.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Ident {
    pub text: String,
    pub span: Span,
}

/// Browser engines accepted by the `browser` option (SPEC 5).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BrowserKind {
    Chromium,
    Firefox,
    Webkit,
}

/// Values of the `reduced-motion` option (SPEC 5): what the page's
/// `prefers-reduced-motion` media query reports.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReducedMotion {
    Reduce,
    NoPreference,
}

/// Automatic dialog responses accepted by the `dialogs` option (SPEC 5).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DialogPolicy {
    Dismiss,
    Accept,
}

/// A `WIDTHxHEIGHT` viewport size in CSS pixels (SPEC 3.1).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Viewport {
    pub width:  u64,
    pub height: u64,
}

/// A typed option value. A literal value is shape-validated at parse time.
/// A value with `{{...}}` interpolation cannot be shape-checked until the
/// runner resolves it at file start (SPEC 11), so it stays a [`Value`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum OptionValue<T> {
    Literal(T),
    Interpolated(Value),
}

/// One `key: value` line in the `[Options]` section, typed per SPEC 5.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum FileOption {
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
    pub fn setup_option(&self) -> Option<&OptionLine> {
        self.options
            .iter()
            .find(|line| matches!(line.option, FileOption::Setup(_)))
    }

    /// The `storage:` option line, when the file has one.
    pub fn storage_option(&self) -> Option<&OptionLine> {
        self.options
            .iter()
            .find(|line| matches!(line.option, FileOption::Storage(_)))
    }
}

/// An `[Options]` line with its source position.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OptionLine {
    pub option: FileOption,
    pub line:   u32,
    pub span:   Span,
}

/// An action line (SPEC 7).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Action {
    pub kind:    ActionKind,
    /// `@duration` step-timeout override (SPEC 12).
    pub timeout: Option<DurationLit>,
    pub line:    u32,
    pub span:    Span,
    /// The step's source text: the line without indentation, trailing
    /// comment, or trailing whitespace. Reports render this.
    pub text:    String,
}

/// The verb and operands of an action (SPEC 7, 17).
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ActionKind {
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
pub enum StoreScope {
    Local,
    Session,
    Cookie,
}

impl StoreScope {
    pub fn keyword(self) -> &'static str {
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
    pub fn default_engine(&self) -> Option<DefaultEngine> {
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
            Self::Visit { .. }
            | Self::Screenshot { .. }
            | Self::Snapshot { .. }
            | Self::Eval { .. }
            | Self::Store { .. } => None,
        }
    }
}

/// A `PAGE` line (SPEC 8).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Page {
    pub check:   PageCheck,
    pub timeout: Option<DurationLit>,
    pub line:    u32,
    pub span:    Span,
    pub text:    String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PageCheck {
    Value(Value),
    Matches(Regex),
}

/// One check line in an `[Asserts]` section (SPEC 9).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Assert {
    pub body:    AssertBody,
    pub timeout: Option<DurationLit>,
    pub line:    u32,
    pub span:    Span,
    pub text:    String,
}

/// The subject and check of an assert. `url` and `title` take string
/// checks only, so the shape is encoded per subject (SPEC 9.3, 17).
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum AssertBody {
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
pub enum StateCheck {
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
pub enum ValueSource {
    Text,
    Value,
    /// `attr:NAME`; the name follows the `attr-name` production.
    Attr(String),
}

/// A string check: operator plus operand (SPEC 9.4).
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum StrCheck {
    Eq(Value),
    Ne(Value),
    Contains(Value),
    Matches(Regex),
}

/// Count comparison operators (SPEC 9.4).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NumOp {
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
}

/// One line in a `[Captures]` section (SPEC 10).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Capture {
    pub name:    Ident,
    pub source:  CaptureSource,
    /// The optional `regex /re/` filter.
    pub filter:  Option<Regex>,
    pub timeout: Option<DurationLit>,
    pub line:    u32,
    pub span:    Span,
    pub text:    String,
}

/// Where a capture's value comes from (SPEC 10).
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CaptureSource {
    Element {
        locator:   Locator,
        extractor: Extractor,
    },
    Url,
    Title,
    Eval(Value),
}

/// Element extractors for captures (SPEC 10).
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Extractor {
    Text,
    Value,
    Count,
    Attr(String),
}

/// One entry: actions, then optional `PAGE`, `[Asserts]`, and
/// `[Captures]`, in that order (SPEC 4).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Entry {
    pub actions:         Vec<Action>,
    pub page:            Option<Page>,
    pub asserts:         Vec<Assert>,
    pub captures:        Vec<Capture>,
    /// The line of the entry's `[Asserts]` header, when the source has
    /// one (it may be present even with zero checks). The formatter uses
    /// it to keep comments on their side of the header.
    pub asserts_header:  Option<u32>,
    /// The line of the entry's `[Captures]` header, when present.
    pub captures_header: Option<u32>,
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
    pub fn line(&self) -> u32 {
        self.first_action().line
    }
}

/// A parsed `.whirl` file.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct File {
    pub path:           PathBuf,
    pub options:        Vec<OptionLine>,
    pub entries:        Vec<Entry>,
    /// Every comment in the file, in source order.
    pub comments:       Vec<Comment>,
    /// The line of the `[Options]` header, when the source has one (it
    /// may be present even with zero option lines).
    pub options_header: Option<u32>,
}

impl File {
    /// The display name of an entry for reports (SPEC 14): the text of the
    /// nearest own-line comment above the entry's first action with no
    /// other step between them (trimmed, without `#`), else the first
    /// action's source text plus its line number.
    pub fn entry_display_name(&self, entry: &Entry) -> String {
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
