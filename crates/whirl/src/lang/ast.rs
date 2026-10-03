//! Typed AST for parsed `.whirl` files (SPEC sections 3-10 and 17).
//!
//! The AST keeps enough source detail (spans, quoting, comment lines, raw
//! step text) for error rendering, report naming, and the later canonical
//! formatter. Values stay unresolved: interpolation segments are split at
//! parse time and resolved by the runner.

use std::fmt;
use std::path::PathBuf;

use crate::check::{FilterKind, PredicateKind, StaticType};

mod options;
pub(crate) mod snapshot;

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
/// written, with its `\/` delimiter escapes; ECMAScript syntax in Unicode
/// mode.
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
    /// `nth:N`, 0-based; a negative N counts from the end. Never the
    /// first segment.
    Nth(i64),
    /// Unprefixed value; legal only in actions (SPEC 6.1). The default
    /// engine (`label:` or `text:`) depends on the action; see
    /// [`ActionKind::default_engine`].
    Default(Value),
    /// `ai:"description"`: the one element a language model finds for the
    /// description (SPEC 6.3). Always the last segment.
    Ai(Value),
    /// An element ref that an `ai:` target resolved to (SPEC 6.3). The
    /// runner puts it in place of the target; no file can write it.
    Ref(String),
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
    Snapshot(snapshot::SnapshotOption),
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
    /// `model: provider/model`, the language model `ACT` asks (SPEC 5,
    /// 7.4).
    Model(Value),
}

impl Locator {
    /// The description of the locator's `ai:` segment, when it has one
    /// (SPEC 6.3).
    pub(crate) fn ai_description(&self) -> Option<&Value> {
        match self.segments.last().map(|segment| &segment.kind) {
            Some(SegmentKind::Ai(description)) => Some(description),
            _ => None,
        }
    }
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

    /// True when any entry has an `ACT` line (SPEC 7.4).
    pub(crate) fn uses_act(&self) -> bool {
        self.entries
            .iter()
            .flat_map(|entry| &entry.actions)
            .any(|action| matches!(action.kind, ActionKind::Act { .. }))
    }

    /// True when any entry has a `GOAL` line (SPEC 7.7).
    pub(crate) fn uses_goal(&self) -> bool {
        self.entries
            .iter()
            .flat_map(|entry| &entry.actions)
            .any(|action| matches!(action.kind, ActionKind::Goal { .. }))
    }

    /// True when any entry has a `JUDGE` line (SPEC 9.8).
    pub(crate) fn uses_judge(&self) -> bool {
        self.entries
            .iter()
            .any(|entry| entry.judges().next().is_some())
    }

    /// True when any line asks a language model: `ACT`, `GOAL`,
    /// `EXTRACT`, `JUDGE`, or a locator with an `ai:` target (SPEC 6.3,
    /// 7.4, 7.6, 7.7, 9.8).
    pub(crate) fn uses_ai(&self) -> bool {
        self.uses_act()
            || self.uses_goal()
            || self.uses_judge()
            || self
                .entries
                .iter()
                .flat_map(|entry| &entry.actions)
                .any(|action| matches!(action.kind, ActionKind::Extract { .. }))
            || self
                .locator_uses()
                .iter()
                .any(|used| used.locator.ai_description().is_some())
    }

    /// Every locator in the file's entries, in line order.
    pub(crate) fn locator_uses(&self) -> Vec<LocatorUse<'_>> {
        let mut uses = Vec::new();
        for entry in &self.entries {
            for action in &entry.actions {
                for locator in action.kind.locators() {
                    uses.push(LocatorUse {
                        locator,
                        count: false,
                    });
                }
            }
            for check in &entry.checks {
                let (locator, count) = match check {
                    CheckStep::Assert(Assert {
                        body: AssertBody::ElementState { locator, .. },
                        ..
                    }) => (locator, false),
                    CheckStep::Assert(Assert {
                        body:
                            AssertBody::Check(CheckLine {
                                subject: Subject::Element { locator, extractor },
                                ..
                            }),
                        ..
                    })
                    | CheckStep::Capture(Capture {
                        subject: Subject::Element { locator, extractor },
                        ..
                    }) => (locator, *extractor == Extractor::Count),
                    _ => continue,
                };
                uses.push(LocatorUse { locator, count });
            }
        }
        uses
    }

    /// True when any entry has a `MOCK` line (SPEC 7.5).
    pub(crate) fn uses_mock(&self) -> bool {
        self.entries
            .iter()
            .flat_map(|entry| &entry.actions)
            .any(|action| matches!(action.kind, ActionKind::Mock { .. }))
    }

    /// The `model:` option line, when the file has one.
    pub(crate) fn model_option(&self) -> Option<&OptionLine> {
        self.options
            .iter()
            .find(|line| matches!(line.option, FileOption::Model(_)))
    }
}

/// One locator of a file's line, for lints (SPEC 6.3).
#[derive(Clone, Copy, Debug)]
pub(crate) struct LocatorUse<'a> {
    pub(crate) locator: &'a Locator,
    /// True when the line counts the locator's matches.
    pub(crate) count:   bool,
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

/// One authored header in an independent HTTP request (SPEC 7.3).
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct HttpHeader {
    pub(crate) name:  String,
    pub(crate) value: Value,
    pub(crate) line:  u32,
}

/// The syntax used for an independent HTTP request body (SPEC 7.3).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum HttpBodyKind {
    Json,
    Text,
}

/// A multiline independent HTTP request body.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct HttpBody {
    pub(crate) kind:     HttpBodyKind,
    /// Interpolation-aware value sent to the shim.
    pub(crate) value:    Value,
    /// Authored body text, without text-body fence delimiters.
    pub(crate) text:     String,
    pub(crate) line:     u32,
    pub(crate) end_line: u32,
}

/// The verb and operands of an action (SPEC 7, 17).
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum ActionKind {
    Http {
        method:  String,
        url:     Value,
        headers: Vec<HttpHeader>,
        body:    Option<HttpBody>,
        /// The complete request text for reports and trace titles.
        source:  String,
    },
    Response {
        name:   Ident,
        method: String,
        url:    Value,
    },
    /// `MOCK METHOD url STATUS` or `MOCK METHOD url failed` serves browser
    /// requests until the file ends (SPEC 7.5).
    Mock {
        method:   String,
        url:      Value,
        response: MockResponse,
        /// The complete mock text for reports and trace titles.
        source:   String,
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
    /// `CLICK`, `RIGHTCLICK`, or `MIDDLECLICK`, by the button it presses.
    Click {
        target: Locator,
        button: MouseButton,
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
    /// `DRAG source to target` drags one element onto another.
    Drag {
        source: Locator,
        target: Locator,
    },
    /// `SCROLL locator` brings the element into view.
    ScrollIntoView {
        target: Locator,
    },
    /// `SCROLL [locator] down` or `SCROLL [locator] to 50%` scrolls the
    /// element's scroll box, or the page without a locator.
    Scroll {
        target: Option<Locator>,
        motion: ScrollMotion,
    },
    /// The value is the path after the `file:` prefix.
    Upload {
        target: Locator,
        path:   Value,
    },
    /// `DROP locator file:path` drops the file on the element. The value
    /// is the path after the `file:` prefix.
    Drop {
        target: Locator,
        path:   Value,
    },
    Screenshot {
        name: Ident,
    },
    /// `SNAPSHOT name [locator]` compares the full page, or only `target`
    /// when it is given (SPEC 7).
    Snapshot {
        name:    Ident,
        target:  Option<Locator>,
        options: Vec<snapshot::SnapshotOptionLine>,
    },
    Eval {
        script: Value,
    },
    /// `ACT [locator] "instruction"` asks the file's model to choose one
    /// element action, inside `scope` when it is given (SPEC 7.4).
    Act {
        scope:       Option<Locator>,
        instruction: Value,
    },
    /// `GOAL "goal"` asks the file's model to reach a goal with several
    /// element actions (SPEC 7.7).
    Goal {
        goal: Value,
    },
    /// `EXTRACT name [locator] "instruction"` asks the file's model to read
    /// a value, shaped by an optional JSON Schema (SPEC 7.6).
    Extract {
        name:        Ident,
        scope:       Option<Locator>,
        instruction: Value,
        schema:      Option<ExtractSchema>,
    },
    /// `STORE local "key" "value"` writes one browser storage entry.
    Store {
        scope: StoreScope,
        key:   Value,
        value: Value,
    },
}

/// What a `MOCK` serves (SPEC 7.5).
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum MockResponse {
    /// A response with a status, header lines, and an optional body.
    Fulfill {
        status:  u16,
        headers: Vec<HttpHeader>,
        body:    Option<HttpBody>,
    },
    /// A failed request, as for a dropped connection.
    Failed,
}

/// The JSON Schema lines below an `EXTRACT` headline (SPEC 7.6), as
/// written. The parser checked that they are one JSON object without
/// `{{ }}`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ExtractSchema {
    pub(crate) text:     String,
    pub(crate) line:     u32,
    pub(crate) end_line: u32,
}

impl ExtractSchema {
    /// The schema as JSON.
    pub(crate) fn json(&self) -> serde_json::Value {
        serde_json::from_str(&self.text).expect("the parser checked that the schema is JSON")
    }
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

/// How `SCROLL` moves its scroll box (SPEC 7).
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum ScrollMotion {
    /// One visible height or width.
    Chunk(ScrollDirection),
    /// A vertical position within the scroll range.
    To(Percent),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ScrollDirection {
    Down,
    Up,
    Left,
    Right,
}

impl ScrollDirection {
    pub(crate) fn keyword(self) -> &'static str {
        match self {
            Self::Down => "down",
            Self::Up => "up",
            Self::Left => "left",
            Self::Right => "right",
        }
    }

    pub(crate) fn from_keyword(text: &str) -> Option<Self> {
        match text {
            "down" => Some(Self::Down),
            "up" => Some(Self::Up),
            "left" => Some(Self::Left),
            "right" => Some(Self::Right),
            _ => None,
        }
    }
}

/// A percent literal from `0%` to `100%`, such as `50%` or `33.5%` (SPEC
/// 3.1). It keeps its digits, so `whirl fmt` writes it as authored.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct Percent {
    digits: String,
}

impl Percent {
    /// Accepts digits with an optional fraction and a `%` suffix, from 0 to
    /// 100.
    pub(crate) fn parse(text: &str) -> Option<Self> {
        let digits = text.strip_suffix('%')?;
        let (whole, fraction) = digits.split_once('.').unwrap_or((digits, "0"));
        let all_digits = |part: &str| !part.is_empty() && part.bytes().all(|b| b.is_ascii_digit());
        if !all_digits(whole) || !all_digits(fraction) {
            return None;
        }
        let value: f64 = digits.parse().ok()?;
        (value <= 100.0).then(|| Self {
            digits: digits.to_owned(),
        })
    }

    pub(crate) fn value(&self) -> f64 {
        self.digits
            .parse()
            .expect("the constructor checked the digits")
    }
}

impl fmt::Display for Percent {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}%", self.digits)
    }
}

/// The mouse button a click verb presses (SPEC 7).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum MouseButton {
    Left,
    Right,
    Middle,
}

impl MouseButton {
    /// The verb that clicks with this button.
    pub(crate) fn keyword(self) -> &'static str {
        match self {
            Self::Left => "CLICK",
            Self::Right => "RIGHTCLICK",
            Self::Middle => "MIDDLECLICK",
        }
    }

    /// The button's name in Playwright and in the `ACT` `click` argument.
    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::Left => "left",
            Self::Right => "right",
            Self::Middle => "middle",
        }
    }

    pub(crate) fn from_name(name: &str) -> Option<Self> {
        match name {
            "left" => Some(Self::Left),
            "right" => Some(Self::Right),
            "middle" => Some(Self::Middle),
            _ => None,
        }
    }
}

impl ActionKind {
    /// The element locators of the action, in order: its targets, the
    /// scope of `ACT`, and the target of `SNAPSHOT`. Snapshot masks are not
    /// targets.
    pub(crate) fn locators(&self) -> Vec<&Locator> {
        match self {
            Self::Click { target, .. }
            | Self::Dblclick { target }
            | Self::Fill { target, .. }
            | Self::Type { target, .. }
            | Self::Check { target }
            | Self::Uncheck { target }
            | Self::Select { target, .. }
            | Self::Hover { target }
            | Self::ScrollIntoView { target }
            | Self::Upload { target, .. }
            | Self::Drop { target, .. } => vec![target],
            Self::Drag { source, target } => vec![source, target],
            Self::Press { target, .. }
            | Self::Scroll { target, .. }
            | Self::Snapshot { target, .. }
            | Self::Act { scope: target, .. }
            | Self::Extract { scope: target, .. } => target.iter().collect(),
            Self::Http { .. }
            | Self::Response { .. }
            | Self::Mock { .. }
            | Self::Popup { .. }
            | Self::Tab { .. }
            | Self::Close { .. }
            | Self::Visit { .. }
            | Self::Screenshot { .. }
            | Self::Eval { .. }
            | Self::Goal { .. }
            | Self::Store { .. } => Vec::new(),
        }
    }

    /// The element locators of the action, mutable, in the order of
    /// [`Self::locators`].
    pub(crate) fn locators_mut(&mut self) -> Vec<&mut Locator> {
        match self {
            Self::Click { target, .. }
            | Self::Dblclick { target }
            | Self::Fill { target, .. }
            | Self::Type { target, .. }
            | Self::Check { target }
            | Self::Uncheck { target }
            | Self::Select { target, .. }
            | Self::Hover { target }
            | Self::ScrollIntoView { target }
            | Self::Upload { target, .. }
            | Self::Drop { target, .. } => vec![target],
            Self::Drag { source, target } => vec![source, target],
            Self::Press { target, .. }
            | Self::Scroll { target, .. }
            | Self::Snapshot { target, .. }
            | Self::Act { scope: target, .. }
            | Self::Extract { scope: target, .. } => target.iter_mut().collect(),
            Self::Http { .. }
            | Self::Response { .. }
            | Self::Mock { .. }
            | Self::Popup { .. }
            | Self::Tab { .. }
            | Self::Close { .. }
            | Self::Visit { .. }
            | Self::Screenshot { .. }
            | Self::Eval { .. }
            | Self::Goal { .. }
            | Self::Store { .. } => Vec::new(),
        }
    }

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
            Self::Click { .. }
            | Self::Dblclick { .. }
            | Self::Hover { .. }
            | Self::Drag { .. }
            | Self::ScrollIntoView { .. }
            | Self::Scroll { .. }
            | Self::Drop { .. } => Some(DefaultEngine::Text),
            Self::Http { .. }
            | Self::Response { .. }
            | Self::Mock { .. }
            | Self::Popup { .. }
            | Self::Tab { .. }
            | Self::Close { .. }
            | Self::Visit { .. }
            | Self::Screenshot { .. }
            | Self::Snapshot { .. }
            | Self::Eval { .. }
            | Self::Act { .. }
            | Self::Goal { .. }
            | Self::Extract { .. }
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

/// One `ASSERT` line (SPEC 9).
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct Assert {
    pub(crate) body:    AssertBody,
    pub(crate) timeout: Option<DurationLit>,
    pub(crate) line:    u32,
    pub(crate) span:    Span,
    pub(crate) text:    String,
}

/// The forms of an `ASSERT` line (SPEC 9.1, 17).
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum AssertBody {
    TabClosed {
        name: Ident,
    },
    ElementState {
        locator: Locator,
        state:   StateCheck,
    },
    Check(CheckLine),
}

/// `subject { filter } [not] predicate` (SPEC 9).
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct CheckLine {
    pub(crate) subject:   Subject,
    pub(crate) filters:   Vec<FilterSpec>,
    pub(crate) negated:   bool,
    pub(crate) predicate: PredicateSpec,
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

/// Where a check or capture reads its value (SPEC 9.2).
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum Subject {
    Element {
        locator:   Locator,
        extractor: Extractor,
    },
    Url,
    Title,
    Eval(Value),
    /// A field of a response: a `RESPONSE` name, or `None` for the
    /// containing HTTP entry's own response.
    Response {
        name:  Option<Ident>,
        field: ResponseField,
    },
    /// A field of the request that a `RESPONSE` name selected (SPEC 9.2).
    Request {
        name:  Ident,
        field: RequestField,
    },
    /// The value an `EXTRACT` line read (SPEC 7.6, 9.2). `text` is true
    /// when that line has no schema, so the value is a string.
    Extract {
        name: Ident,
        text: bool,
    },
}

impl Subject {
    /// The subject's static type (SPEC 9.2), before any filter.
    pub(crate) fn static_type(&self) -> StaticType {
        match self {
            Self::Element {
                extractor: Extractor::Count,
                ..
            }
            | Self::Response {
                field: ResponseField::Status,
                ..
            } => StaticType::NUMBER,
            Self::Element { .. }
            | Self::Url
            | Self::Title
            | Self::Response {
                field: ResponseField::Header(_) | ResponseField::Location | ResponseField::Body,
                ..
            }
            | Self::Request {
                field:
                    RequestField::Method
                    | RequestField::Url
                    | RequestField::Header(_)
                    | RequestField::Body,
                ..
            }
            | Self::Extract { text: true, .. } => StaticType::STRING,
            Self::Response {
                field: ResponseField::Bytes,
                ..
            }
            | Self::Request {
                field: RequestField::Bytes,
                ..
            } => StaticType::BYTES,
            Self::Eval(_)
            | Self::Response {
                field: ResponseField::Json(_) | ResponseField::Xpath(_),
                ..
            }
            | Self::Request {
                field: RequestField::Json(_) | RequestField::Xpath(_),
                ..
            }
            | Self::Extract { text: false, .. } => StaticType::Any,
        }
    }
}

/// A field of one HTTP response (SPEC 9.2).
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum ResponseField {
    Status,
    Header(Value),
    Location,
    Body,
    Bytes,
    /// `json:PATH`, short for `body json:PATH`.
    Json(Value),
    /// `xpath:EXPR`, short for `body xpath:EXPR`.
    Xpath(Value),
}

/// A field of one observed request (SPEC 9.2).
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum RequestField {
    Method,
    Url,
    Header(Value),
    Body,
    Bytes,
    /// `json:PATH`, short for `body json:PATH`.
    Json(Value),
    /// `xpath:EXPR`, short for `body xpath:EXPR`.
    Xpath(Value),
}

/// Element extractors (SPEC 9.2).
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum Extractor {
    Text,
    Value,
    Count,
    Attr(String),
}

/// One filter with its source arguments (SPEC 9.5).
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct FilterSpec {
    pub(crate) kind: FilterKind,
    pub(crate) args: Vec<FilterArg>,
    pub(crate) span: Span,
}

/// A filter argument as written.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum FilterArg {
    Value(Value),
    Regex(Regex),
    Index(i64),
}

/// A predicate with its source operand (SPEC 9.4).
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum PredicateSpec {
    Compare {
        kind:     PredicateKind,
        expected: Operand,
    },
    Matches(Regex),
    Word(PredicateKind),
}

impl PredicateSpec {
    pub(crate) fn kind(&self) -> PredicateKind {
        match self {
            Self::Compare { kind, .. } | Self::Word(kind) => *kind,
            Self::Matches(_) => PredicateKind::Matches,
        }
    }
}

/// An expected value: a value, or a single-line JSON literal (SPEC 3.1).
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum Operand {
    Value(Value),
    Json(JsonLiteral),
}

/// A JSON array or object written on the check line. `value` holds the
/// authored text split into interpolation segments.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct JsonLiteral {
    pub(crate) text:  String,
    pub(crate) value: Value,
}

/// The static type after a subject and its filters, or the first filter
/// that cannot take its input with that input's type.
pub(crate) fn chain_type<'a>(
    subject: &Subject,
    filters: &'a [FilterSpec],
) -> Result<StaticType, (&'a FilterSpec, StaticType)> {
    let mut current = subject.static_type();
    for filter in filters {
        current = filter.kind.output(&current).ok_or((filter, current))?;
    }
    Ok(current)
}

/// One `CAPTURE` line (SPEC 10).
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct Capture {
    pub(crate) name:    Ident,
    pub(crate) subject: Subject,
    pub(crate) filters: Vec<FilterSpec>,
    pub(crate) timeout: Option<DurationLit>,
    pub(crate) line:    u32,
    pub(crate) span:    Span,
    pub(crate) text:    String,
}

/// One check line of an entry (SPEC 9, 10): an `ASSERT`, a `JUDGE`, or a
/// `CAPTURE`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum CheckStep {
    Assert(Assert),
    Judge(Judge),
    Capture(Capture),
}

impl CheckStep {
    pub(crate) fn line(&self) -> u32 {
        match self {
            Self::Assert(assert) => assert.line,
            Self::Judge(judge) => judge.line,
            Self::Capture(capture) => capture.line,
        }
    }

    /// The line's source text.
    pub(crate) fn text(&self) -> &str {
        match self {
            Self::Assert(assert) => &assert.text,
            Self::Judge(judge) => &judge.text,
            Self::Capture(capture) => &capture.text,
        }
    }
}

/// A `JUDGE [locator] "claim"` line (SPEC 9.8).
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct Judge {
    /// The element the model sees; `None` for the page.
    pub(crate) scope:   Option<Locator>,
    pub(crate) claim:   Value,
    pub(crate) timeout: Option<DurationLit>,
    pub(crate) line:    u32,
    pub(crate) span:    Span,
    pub(crate) text:    String,
}

/// One entry: actions, then an optional `PAGE` line, then check lines in
/// the order written (SPEC 4).
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct Entry {
    pub(crate) actions:  Vec<Action>,
    pub(crate) page:     Option<Page>,
    pub(crate) checks:   Vec<CheckStep>,
    /// The spans of the entry's deprecated `[Asserts]` and `[Captures]`
    /// section headers, in source order (SPEC 4). `whirl fmt` drops them
    /// and writes each check as an `ASSERT` or `CAPTURE` line.
    pub(crate) sections: Vec<Span>,
}

impl Entry {
    /// The entry's `ASSERT` lines, in source order.
    pub(crate) fn asserts(&self) -> impl Iterator<Item = &Assert> {
        self.checks.iter().filter_map(|check| match check {
            CheckStep::Assert(assert) => Some(assert),
            CheckStep::Judge(_) | CheckStep::Capture(_) => None,
        })
    }

    /// The entry's `CAPTURE` lines, in source order.
    pub(crate) fn captures(&self) -> impl Iterator<Item = &Capture> {
        self.checks.iter().filter_map(|check| match check {
            CheckStep::Capture(capture) => Some(capture),
            CheckStep::Assert(_) | CheckStep::Judge(_) => None,
        })
    }

    /// The entry's `JUDGE` lines, in source order.
    pub(crate) fn judges(&self) -> impl Iterator<Item = &Judge> {
        self.checks.iter().filter_map(|check| match check {
            CheckStep::Judge(judge) => Some(judge),
            CheckStep::Assert(_) | CheckStep::Capture(_) => None,
        })
    }

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
            let checks = entry.checks.iter().map(CheckStep::line);
            actions.chain(page).chain(checks)
        });
        option_lines.chain(entry_lines)
    }
}
