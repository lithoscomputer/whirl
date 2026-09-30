//! Line-oriented parser for `.whirl` files (SPEC sections 3-10, 16, 17).
//!
//! [`parse_file`] parses one file and stops at that file's first error.
//! The CLI parses every input and reports each file's error, so
//! `whirl check` can surface all broken files in one pass (SPEC 13).

use std::collections::HashMap;
use std::fmt::{self, Write as _};
use std::iter::Peekable;
use std::mem;
use std::path::{Path, PathBuf};
use std::vec::IntoIter;

use crate::check::{
    COMPARE_KEYWORDS, Charset, DateFormat, FILTER_KEYWORDS, FilterKind, JsonQuery, Pattern,
    PatternFlags, WORD_PREDICATES, XpathQuery, bytes_literal, is_bytes_literal_shape, quote_json,
};
use crate::lang::ast::snapshot::{MaxDiff, PixelThreshold, SnapshotOption, SnapshotOptionLine};
use crate::lang::ast::{
    Action, ActionKind, Assert, AssertBody, BrowserKind, Capture, CheckLine, CheckStep, Comment,
    DialogPolicy, DurationLit, Entry, ExtractSchema, Extractor, File, FileOption, FilterArg,
    FilterSpec, HttpBody, HttpBodyKind, HttpHeader, Ident, JsonLiteral, Judge, Locator,
    LocatorSegment, MockResponse, MouseButton, Operand, OptionLine, OptionValue, Page, PageCheck,
    Percent, PredicateSpec, ReducedMotion, Regex, RegexFlags, RequestField, ResponseField,
    ScrollDirection, ScrollMotion, SegmentKind, Span, StateCheck, StoreScope, Subject, TextPrefix,
    Value, ValueSegment, Viewport, chain_type,
};

/// A parse diagnostic (SPEC 16): file, line, column, the source line, a
/// caret under the offending token, and the expected alternatives.
#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
#[error("{}", self.render())]
pub(crate) struct ParseError {
    /// The stable diagnostic code (SPEC 16).
    pub(crate) code:        ParseErrorCode,
    pub(crate) path:        PathBuf,
    /// 1-based line of the offending token.
    pub(crate) line:        u32,
    /// 1-based character column of the offending token.
    pub(crate) column:      u32,
    /// Length of the offending token in characters (caret width).
    pub(crate) len:         u32,
    /// The full source line, without its line ending.
    pub(crate) source_line: String,
    pub(crate) message:     String,
    /// Expected alternatives, possibly empty.
    pub(crate) expected:    Vec<String>,
}

impl ParseError {
    /// Renders the diagnostic: location and message, the source line, a
    /// caret under the offending token, and the expected alternatives.
    pub(crate) fn render(&self) -> String {
        let mut out = String::new();
        let location = format!("{}:{}:{}", self.path.display(), self.line, self.column);
        let _ = writeln!(out, "{location}: error: {}", self.message);
        let _ = writeln!(out, "  {}", self.source_line);
        let pad = usize::try_from(self.column.saturating_sub(1)).unwrap_or(0);
        let width = usize::try_from(self.len.max(1)).unwrap_or(1);
        let _ = write!(out, "  {}{}", " ".repeat(pad), "^".repeat(width));
        match self.expected.as_slice() {
            [] => {}
            [one] => {
                let _ = write!(out, "\n  expected {one}");
            }
            many => {
                let _ = write!(out, "\n  expected one of: {}", many.join(", "));
            }
        }
        out
    }
}

/// The stable code of a parse diagnostic (SPEC 16).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ParseErrorCode {
    /// An ordinary syntax error.
    Syntax,
}

impl ParseErrorCode {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Syntax => "parse-error",
        }
    }
}

/// A diagnostic local to one line; the parser adds file and line context.
struct LineError {
    line:     Option<u32>,
    column:   u32,
    len:      u32,
    source:   Option<String>,
    message:  String,
    expected: Vec<String>,
}

impl LineError {
    fn new(span: Span, message: impl Into<String>) -> Self {
        Self {
            line:     None,
            column:   span.column,
            len:      span.len,
            source:   None,
            message:  message.into(),
            expected: Vec::new(),
        }
    }

    fn expecting<S: fmt::Display>(mut self, expected: impl IntoIterator<Item = S>) -> Self {
        self.expected = expected.into_iter().map(|item| item.to_string()).collect();
        self
    }

    fn at_source(mut self, line: u32, source: impl Into<String>) -> Self {
        self.line = Some(line);
        self.source = Some(source.into());
        self
    }
}

fn is_ident(text: &str) -> bool {
    let mut chars = text.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    (first.is_ascii_alphabetic() || first == '_')
        && chars.all(|ch| ch.is_ascii_alphanumeric() || ch == '_')
}

fn is_attr_name(text: &str) -> bool {
    let mut chars = text.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    (first.is_ascii_alphabetic() || first == '_')
        && chars.all(|ch| ch.is_ascii_alphanumeric() || ch == '_' || ch == '-')
}

/// Reads a bare token that starts with `@`: always a step timeout, since
/// a bare value cannot start with `@` (SPEC 3.1).
fn timeout_token(text: &str, span: Span) -> Result<DurationLit, LineError> {
    let rest = &text[1..];
    if let Ok(duration) = rest.parse::<DurationLit>() {
        return Ok(duration);
    }
    let digits = rest.trim_end_matches(['m', 's']);
    let message = if !digits.is_empty()
        && digits.bytes().all(|byte| byte.is_ascii_digit())
        && matches!(&rest[digits.len()..], "ms" | "s")
    {
        "the step timeout is too long"
    } else {
        "a bare value cannot start with `@`; quote it"
    };
    Err(LineError::new(span, message).expecting(["a duration like @10s", "a quoted value"]))
}

/// One whitespace-free run of source (SPEC 3.1): a quoted string, a bare
/// run, or a bare run that ends in `:` joined to a quoted string, such as
/// `placeholder:"Search products"`.
#[derive(Clone, Debug)]
struct RawToken {
    parts:   Vec<RawPart>,
    span:    Span,
    /// The step timeout, when the token is a bare `@duration`.
    timeout: Option<DurationLit>,
}

#[derive(Clone, Debug)]
enum RawPart {
    /// A bare run, kept raw; interpolation splits on demand.
    Bare { text: String, column: u32 },
    /// A quoted string, unescaped and interpolation-split at scan time.
    Quoted { segments: Vec<ValueSegment> },
}

impl RawToken {
    /// The token's text when it is a single bare run: candidate keyword,
    /// section header, prefix segment, or timeout suffix. A quoted token
    /// is always a value (SPEC 3.1), so this returns `None` for it.
    fn bare_single(&self) -> Option<&str> {
        match self.parts.as_slice() {
            [RawPart::Bare { text, .. }] => Some(text),
            _ => None,
        }
    }

    /// Converts the token to a value, splitting bare interpolation. A step
    /// timeout is never a value.
    fn into_value(self) -> Result<Value, LineError> {
        if self.timeout.is_some() {
            return Err(
                LineError::new(self.span, "a step timeout must end the line")
                    .expecting(["a value", "end of line"]),
            );
        }
        let mut segments = Vec::new();
        let mut quoted = false;
        for part in self.parts {
            match part {
                RawPart::Bare { text, column } => {
                    segments.extend(bare_segments(&text, column)?);
                }
                RawPart::Quoted {
                    segments: quoted_segments,
                } => {
                    quoted = true;
                    segments.extend(quoted_segments);
                }
            }
        }
        Ok(Value {
            segments: merge_literals(segments),
            span: self.span,
            quoted,
        })
    }
}

/// Merges adjacent literal segments produced by part joins.
fn merge_literals(segments: Vec<ValueSegment>) -> Vec<ValueSegment> {
    let mut merged: Vec<ValueSegment> = Vec::with_capacity(segments.len());
    for segment in segments {
        match (merged.last_mut(), segment) {
            (Some(ValueSegment::Literal(last)), ValueSegment::Literal(text)) => {
                last.push_str(&text);
            }
            (_, segment) => merged.push(segment),
        }
    }
    if merged.is_empty() {
        merged.push(ValueSegment::Literal(String::new()));
    }
    merged
}

/// Splits a bare run into interpolation segments: `{{name}}` references,
/// `\{{` for a literal `{{` (SPEC 11).
fn bare_segments(text: &str, column: u32) -> Result<Vec<ValueSegment>, LineError> {
    let chars: Vec<char> = text.chars().collect();
    let mut segments = Vec::new();
    let mut literal = String::new();
    let mut pos = 0;
    while pos < chars.len() {
        let ch = chars[pos];
        if ch == '\\' && chars.get(pos + 1) == Some(&'{') {
            literal.push('{');
            pos += 2;
        } else if ch == '{' && chars.get(pos + 1) == Some(&'{') {
            if !literal.is_empty() {
                segments.push(ValueSegment::Literal(mem::take(&mut literal)));
            }
            let at = column + u32::try_from(pos).unwrap_or(u32::MAX);
            let (segment, used) = scan_var_ref(&chars[pos..], at)?;
            segments.push(segment);
            pos += used;
        } else {
            literal.push(ch);
            pos += 1;
        }
    }
    if !literal.is_empty() || segments.is_empty() {
        segments.push(ValueSegment::Literal(literal));
    }
    Ok(segments)
}

/// Splits one line of JSON into interpolation segments (SPEC 7.3, 11), as
/// the runner reads it: `\{{` writes a literal `{{`, and inside a string a
/// backslash escapes the next character, so `\\{{name}}` is an escaped
/// backslash before a reference.
fn json_segments(text: &str, column: u32) -> Result<Vec<ValueSegment>, LineError> {
    let chars: Vec<char> = text.chars().collect();
    let mut segments = Vec::new();
    let mut literal = String::new();
    let mut in_string = false;
    let mut escaped = false;
    let mut pos = 0;
    while pos < chars.len() {
        let ch = chars[pos];
        if ch == '\\'
            && !escaped
            && chars.get(pos + 1) == Some(&'{')
            && chars.get(pos + 2) == Some(&'{')
        {
            literal.push_str("{{");
            pos += 3;
            continue;
        }
        if ch == '{' && chars.get(pos + 1) == Some(&'{') {
            if !literal.is_empty() {
                segments.push(ValueSegment::Literal(mem::take(&mut literal)));
            }
            let at = column + u32::try_from(pos).unwrap_or(u32::MAX);
            let (segment, used) = scan_var_ref(&chars[pos..], at)?;
            segments.push(segment);
            pos += used;
            continue;
        }
        literal.push(ch);
        if in_string {
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == '"' {
                in_string = false;
            }
        } else if ch == '"' {
            in_string = true;
        }
        pos += 1;
    }
    if !literal.is_empty() || segments.is_empty() {
        segments.push(ValueSegment::Literal(literal));
    }
    Ok(segments)
}

/// Scans a `{{name}}` or `{{env.NAME}}` reference starting at `{{`.
/// Returns the segment and the number of characters consumed.
fn scan_var_ref(chars: &[char], column: u32) -> Result<(ValueSegment, usize), LineError> {
    let close = chars
        .windows(2)
        .position(|pair| pair == ['}', '}'])
        .ok_or_else(|| {
            let span = Span {
                line: 0,
                column,
                len: 2,
            };
            LineError::new(span, "unterminated `{{` variable reference")
                .expecting(["`}}` to close the reference"])
        })?;
    let name: String = chars[2..close].iter().collect();
    let span = Span {
        line: 0,
        column,
        len: u32::try_from(close + 2).unwrap_or(u32::MAX),
    };
    let segment = if let Some(env_name) = name.strip_prefix("env.") {
        if !is_ident(env_name) {
            return Err(LineError::new(
                span,
                format!("invalid variable reference `{{{{{name}}}}}`"),
            )
            .expecting(["a variable name like {{env.NAME}}"]));
        }
        ValueSegment::EnvVar(env_name.to_owned())
    } else if let Some(setup_name) = name.strip_prefix("setup.") {
        if !is_ident(setup_name) {
            return Err(LineError::new(
                span,
                format!("invalid variable reference `{{{{{name}}}}}`"),
            )
            .expecting(["a capture name like {{setup.name}}"]));
        }
        ValueSegment::SetupVar(setup_name.to_owned())
    } else if is_ident(&name) {
        ValueSegment::Var(name)
    } else {
        return Err(
            LineError::new(span, format!("invalid variable reference `{{{{{name}}}}}`"))
                .expecting(["a variable name like {{name}}, {{env.NAME}}, or {{setup.name}}"]),
        );
    };
    Ok((segment, close + 2))
}

/// A scanner over one source line. Columns are 1-based characters.
struct Cursor {
    chars:   Vec<char>,
    pos:     usize,
    line_no: u32,
}

impl Cursor {
    fn new(line: &str, line_no: u32) -> Self {
        Self {
            chars: line.chars().collect(),
            pos: 0,
            line_no,
        }
    }

    fn column(&self) -> u32 {
        u32::try_from(self.pos + 1).unwrap_or(u32::MAX)
    }

    fn peek(&self) -> Option<char> {
        self.chars.get(self.pos).copied()
    }

    fn skip_ws(&mut self) {
        while self.peek().is_some_and(char::is_whitespace) {
            self.pos += 1;
        }
    }

    /// True when a comment starts at the cursor: a `#` at the start of the
    /// line or after white space (SPEC 3). Inside a token, `#` is text.
    fn at_comment(&self) -> bool {
        self.peek() == Some('#')
            && self
                .pos
                .checked_sub(1)
                .is_none_or(|before| self.chars[before].is_whitespace())
    }

    /// The trailing comment at the cursor, if the rest of the line is one.
    fn take_comment(&mut self) -> Option<Comment> {
        self.skip_ws();
        if !self.at_comment() {
            return None;
        }
        let column = self.column();
        let text: String = self.chars[self.pos + 1..].iter().collect();
        self.pos = self.chars.len();
        Some(Comment {
            line: self.line_no,
            column,
            text,
            own_line: false,
        })
    }

    /// True when only whitespace or a comment remains.
    fn at_line_end(&mut self) -> bool {
        self.skip_ws();
        self.peek().is_none() || self.at_comment()
    }

    fn span_from(&self, start: usize) -> Span {
        Span {
            line:   self.line_no,
            column: u32::try_from(start + 1).unwrap_or(u32::MAX),
            len:    u32::try_from(self.pos - start).unwrap_or(u32::MAX),
        }
    }

    /// Scans the next token, or `None` at the end of the line or at a
    /// comment (SPEC 3.1). Bare runs end at whitespace or `"`; a `#` inside
    /// a token is text. A quoted string joins only a bare run that ends in
    /// `:` or `:~`, and nothing joins a closing quote. A bare token that
    /// starts with `@` is a step timeout.
    fn next_token(&mut self) -> Result<Option<RawToken>, LineError> {
        if self.at_line_end() {
            return Ok(None);
        }
        let start = self.pos;
        let mut parts = Vec::new();
        if self.peek() == Some('"') {
            parts.push(self.scan_quoted()?);
        } else {
            let bare = self.scan_bare();
            let prefix = matches!(
                &bare,
                RawPart::Bare { text, .. } if text.ends_with(':') || text.ends_with(":~")
            );
            parts.push(bare);
            if prefix && self.peek() == Some('"') {
                parts.push(self.scan_quoted()?);
            }
        }
        if let Some(ch) = self.peek()
            && !ch.is_whitespace()
        {
            let span = Span {
                line:   self.line_no,
                column: self.column(),
                len:    1,
            };
            let message = if matches!(parts.last(), Some(RawPart::Quoted { .. })) {
                "expected white space after the closing quote"
            } else {
                "a quote can follow only a prefix such as `label:`; quote the whole value"
            };
            return Err(LineError::new(span, message).expecting(["white space"]));
        }
        let span = self.span_from(start);
        let timeout = match parts.as_slice() {
            [RawPart::Bare { text, .. }] if text.starts_with('@') => {
                Some(timeout_token(text, span)?)
            }
            [RawPart::Bare { text, .. }, _] if text.starts_with('@') => {
                return Err(
                    LineError::new(span, "a bare value cannot start with `@`; quote it")
                        .expecting(["a quoted value"]),
                );
            }
            _ => None,
        };
        Ok(Some(RawToken {
            parts,
            span,
            timeout,
        }))
    }

    fn scan_bare(&mut self) -> RawPart {
        let start = self.pos;
        let column = self.column();
        while let Some(ch) = self.peek() {
            if ch.is_whitespace() || ch == '"' {
                break;
            }
            self.pos += 1;
        }
        let text: String = self.chars[start..self.pos].iter().collect();
        RawPart::Bare { text, column }
    }

    /// Scans a quoted string starting at `"`, applying escapes and
    /// splitting interpolation (SPEC 3.1, 11).
    fn scan_quoted(&mut self) -> Result<RawPart, LineError> {
        let open = self.pos;
        self.pos += 1;
        let mut segments = Vec::new();
        let mut literal = String::new();
        loop {
            let column = self.column();
            match self.peek() {
                None => {
                    let span = self.span_from(open);
                    return Err(LineError::new(span, "unterminated string")
                        .expecting(["`\"` to close the string"]));
                }
                Some('"') => {
                    self.pos += 1;
                    break;
                }
                Some('\\') => {
                    self.pos += 1;
                    literal.push(self.scan_escape(column)?);
                }
                Some('{') if self.chars.get(self.pos + 1) == Some(&'{') => {
                    if !literal.is_empty() {
                        segments.push(ValueSegment::Literal(mem::take(&mut literal)));
                    }
                    let (segment, used) = scan_var_ref(&self.chars[self.pos..], column)?;
                    segments.push(segment);
                    self.pos += used;
                }
                Some(ch) => {
                    literal.push(ch);
                    self.pos += 1;
                }
            }
        }
        if !literal.is_empty() || segments.is_empty() {
            segments.push(ValueSegment::Literal(literal));
        }
        Ok(RawPart::Quoted { segments })
    }

    /// Scans the character after a backslash inside a quoted string.
    fn scan_escape(&mut self, column: u32) -> Result<char, LineError> {
        let expected = ["\\\"", "\\\\", "\\n", "\\t", "\\u{XXXX}", "\\{{"];
        let invalid = |len: u32, text: String| {
            let span = Span {
                line: 0,
                column,
                len,
            };
            LineError::new(span, format!("invalid escape `{text}`")).expecting(expected)
        };
        let Some(ch) = self.peek() else {
            return Err(invalid(1, "\\".to_owned()));
        };
        self.pos += 1;
        match ch {
            '"' => Ok('"'),
            '\\' => Ok('\\'),
            'n' => Ok('\n'),
            't' => Ok('\t'),
            '{' => Ok('{'),
            'u' => {
                if self.peek() != Some('{') {
                    return Err(invalid(2, "\\u".to_owned()));
                }
                self.pos += 1;
                let digits_start = self.pos;
                while self.peek().is_some_and(|ch| ch.is_ascii_hexdigit()) {
                    self.pos += 1;
                }
                let digits: String = self.chars[digits_start..self.pos].iter().collect();
                if digits.is_empty() || self.peek() != Some('}') {
                    let len = u32::try_from(self.pos + 1 - (digits_start - 3)).unwrap_or(2);
                    return Err(invalid(len, format!("\\u{{{digits}")));
                }
                self.pos += 1;
                let code = u32::from_str_radix(&digits, 16)
                    .ok()
                    .and_then(char::from_u32);
                code.ok_or_else(|| {
                    let len = u32::try_from(digits.chars().count() + 4).unwrap_or(2);
                    invalid(len, format!("\\u{{{digits}}}"))
                })
            }
            other => Err(invalid(2, format!("\\{other}"))),
        }
    }

    /// The step text and span from `start` to the current position,
    /// with trailing whitespace trimmed.
    fn content(&self, start: usize) -> (String, Span) {
        let mut end = self.pos.min(self.chars.len());
        while end > start && self.chars[end - 1].is_whitespace() {
            end -= 1;
        }
        let text: String = self.chars[start..end].iter().collect();
        let span = Span {
            line:   self.line_no,
            column: u32::try_from(start + 1).unwrap_or(u32::MAX),
            len:    u32::try_from(end - start).unwrap_or(u32::MAX),
        };
        (text, span)
    }

    /// Scans a single-line JSON literal starting at `[` or `{` (SPEC 3.1):
    /// it ends where its outer delimiter closes. A `{{name}}` reference
    /// outside a string stands for a JSON value.
    fn scan_json_literal(&mut self) -> Result<JsonLiteral, LineError> {
        let start = self.pos;
        let column = self.column();
        let mut stack = Vec::new();
        let mut in_string = false;
        let mut escaped = false;
        while let Some(ch) = self.peek() {
            self.pos += 1;
            if in_string {
                if escaped {
                    escaped = false;
                } else if ch == '\\' {
                    escaped = true;
                } else if ch == '"' {
                    in_string = false;
                }
                continue;
            }
            match ch {
                '"' => in_string = true,
                '{' if self.peek() == Some('{') => {
                    let close = self.chars[self.pos..]
                        .windows(2)
                        .position(|pair| pair == ['}', '}']);
                    let Some(close) = close else {
                        let span = self.span_from(start);
                        return Err(LineError::new(span, "unterminated `{{` variable reference"));
                    };
                    self.pos += close + 2;
                }
                '{' | '[' => stack.push(if ch == '{' { '}' } else { ']' }),
                '}' | ']' => {
                    if stack.pop() != Some(ch) {
                        let span = self.span_from(start);
                        return Err(LineError::new(span, "mismatched delimiter in JSON literal"));
                    }
                    if stack.is_empty() {
                        break;
                    }
                }
                _ => {}
            }
        }
        let span = self.span_from(start);
        if !stack.is_empty() {
            return Err(LineError::new(span, "unterminated JSON literal")
                .expecting(["a JSON array or object that ends on this line"]));
        }
        let text: String = self.chars[start..self.pos].iter().collect();
        let validation = json_for_validation(&text)?;
        if let Err(error) = serde_json::from_str::<serde_json::Value>(&validation) {
            return Err(LineError::new(
                span,
                format!("invalid JSON literal: {error}"),
            ));
        }
        let segments = json_segments(&text, column)?;
        Ok(JsonLiteral {
            value: Value {
                segments: merge_literals(segments),
                span,
                quoted: false,
            },
            text,
        })
    }

    /// Scans a `/pattern/flags` regex literal (SPEC 3.1). Only called
    /// where the grammar expects a regex: after `matches` and after
    /// `regex` in captures.
    fn expect_regex(&mut self) -> Result<Regex, LineError> {
        self.skip_ws();
        let start = self.pos;
        if self.peek() != Some('/') {
            let span = Span {
                line:   self.line_no,
                column: self.column(),
                len:    1,
            };
            return Err(LineError::new(span, "expected a regex literal")
                .expecting(["a regex like /pattern/"]));
        }
        self.pos += 1;
        let mut pattern = String::new();
        loop {
            match self.peek() {
                None => {
                    let span = self.span_from(start);
                    return Err(LineError::new(span, "unterminated regex")
                        .expecting(["`/` to close the regex"]));
                }
                Some('\\') if self.chars.get(self.pos + 1) == Some(&'/') => {
                    pattern.push_str("\\/");
                    self.pos += 2;
                }
                Some('\\') => {
                    pattern.push('\\');
                    if let Some(next) = self.chars.get(self.pos + 1) {
                        pattern.push(*next);
                        self.pos += 2;
                    } else {
                        self.pos += 1;
                    }
                }
                Some('/') => {
                    self.pos += 1;
                    break;
                }
                Some(ch) => {
                    pattern.push(ch);
                    self.pos += 1;
                }
            }
        }
        let mut flags = RegexFlags::default();
        while let Some(ch) = self.peek() {
            if ch.is_whitespace() {
                break;
            }
            let column = self.column();
            match ch {
                'i' => flags.ignore_case = true,
                's' => flags.dot_all = true,
                'm' => flags.multiline = true,
                other => {
                    let span = Span {
                        line: self.line_no,
                        column,
                        len: 1,
                    };
                    return Err(
                        LineError::new(span, format!("invalid regex flag `{other}`"))
                            .expecting(["i", "s", "m"]),
                    );
                }
            }
            self.pos += 1;
        }
        Ok(Regex {
            pattern,
            flags,
            span: self.span_from(start),
        })
    }
}

/// The ARIA roles that are locator prefixes, such as `button:` (SPEC
/// 6.1): every role the pinned Playwright accepts, except `generic`,
/// `none`, and `presentation`.
pub(crate) const ROLES: [&str; 79] = [
    "alert",
    "alertdialog",
    "application",
    "article",
    "banner",
    "blockquote",
    "button",
    "caption",
    "cell",
    "checkbox",
    "code",
    "columnheader",
    "combobox",
    "complementary",
    "contentinfo",
    "definition",
    "deletion",
    "dialog",
    "directory",
    "document",
    "emphasis",
    "feed",
    "figure",
    "form",
    "grid",
    "gridcell",
    "group",
    "heading",
    "img",
    "insertion",
    "link",
    "list",
    "listbox",
    "listitem",
    "log",
    "main",
    "marquee",
    "math",
    "meter",
    "menu",
    "menubar",
    "menuitem",
    "menuitemcheckbox",
    "menuitemradio",
    "navigation",
    "note",
    "option",
    "paragraph",
    "progressbar",
    "radio",
    "radiogroup",
    "region",
    "row",
    "rowgroup",
    "rowheader",
    "scrollbar",
    "search",
    "searchbox",
    "separator",
    "slider",
    "spinbutton",
    "status",
    "strong",
    "subscript",
    "superscript",
    "switch",
    "tab",
    "table",
    "tablist",
    "tabpanel",
    "term",
    "textbox",
    "time",
    "timer",
    "toolbar",
    "tooltip",
    "tree",
    "treegrid",
    "treeitem",
];

/// True when `text` names a role prefix, such as `button`.
pub(crate) fn is_role(text: &str) -> bool {
    ROLES.contains(&text)
}

/// The segment prefixes, for diagnostics (SPEC 6.1).
const SEGMENT_PREFIXES: [&str; 11] = [
    "a role such as button:",
    "label:",
    "placeholder:",
    "text:",
    "alt:",
    "title:",
    "testid:",
    "css:",
    "frame:",
    "nth:",
    "ai:",
];

const TEXT_PREFIXES: [(&str, TextPrefix); 5] = [
    ("label", TextPrefix::Label),
    ("placeholder", TextPrefix::Placeholder),
    ("text", TextPrefix::Text),
    ("alt", TextPrefix::Alt),
    ("title", TextPrefix::Title),
];

/// Drops `prefix_len` ASCII characters from the front of a token's first
/// (bare) part. Returns `None` when nothing remains.
fn strip_prefix_token(token: RawToken, prefix_len: usize) -> Option<RawToken> {
    let mut parts = token.parts.into_iter();
    let Some(RawPart::Bare { text, column }) = parts.next() else {
        return None;
    };
    let rest = text.get(prefix_len..).unwrap_or("");
    let mut new_parts = Vec::new();
    if !rest.is_empty() {
        let column = column + u32::try_from(prefix_len).unwrap_or(u32::MAX);
        new_parts.push(RawPart::Bare {
            text: rest.to_owned(),
            column,
        });
    }
    new_parts.extend(parts);
    if new_parts.is_empty() {
        return None;
    }
    let offset = u32::try_from(prefix_len).unwrap_or(u32::MAX);
    let span = Span {
        line:   token.span.line,
        column: token.span.column + offset,
        len:    token.span.len.saturating_sub(offset),
    };
    Some(RawToken {
        parts: new_parts,
        span,
        timeout: None,
    })
}

/// The prefix of a token that starts with a bare part holding a colon:
/// the text before the first `:`, and the colon's byte index. In a
/// locator, a bare colon always marks a prefix (SPEC 6.1).
fn segment_prefix(token: &RawToken) -> Option<(String, usize)> {
    let Some(RawPart::Bare { text, .. }) = token.parts.first() else {
        return None;
    };
    let colon = text.find(':')?;
    Some((text[..colon].to_owned(), colon))
}

/// Parses one locator segment from one token (SPEC 6, 17): `prefix:value`,
/// or unprefixed text where `allow_default` holds.
fn parse_segment(
    token: RawToken,
    allow_default: bool,
    is_first: bool,
) -> Result<LocatorSegment, LineError> {
    let span = token.span;
    let Some((name, colon)) = segment_prefix(&token) else {
        return unprefixed_segment(token, allow_default);
    };
    let name = name.as_str();
    let rest = strip_prefix_token(token, colon + 1);
    let missing =
        |what: &str| LineError::new(span, format!("`{name}:` needs {what}")).expecting([what]);
    let text_prefix = TEXT_PREFIXES.iter().find(|(text, _)| *text == name);
    if text_prefix.is_some() || is_role(name) {
        let rest = rest.ok_or_else(|| missing("a value"))?;
        let (substring, rest) = match_mode(rest).ok_or_else(|| missing("a value after `~`"))?;
        let kind = if let Some((_, prefix)) = text_prefix {
            SegmentKind::TextEngine {
                prefix: *prefix,
                substring,
                value: rest.into_value()?,
            }
        } else {
            let any = rest.bare_single() == Some("*");
            if any && substring {
                return Err(
                    LineError::new(span, format!("`{name}:~` needs a name")).expecting(["a name"])
                );
            }
            SegmentKind::Role {
                substring,
                role: name.to_owned(),
                name: if any { None } else { Some(rest.into_value()?) },
            }
        };
        return Ok(LocatorSegment { kind, span });
    }
    if !matches!(name, "testid" | "css" | "frame" | "ai" | "nth") {
        return Err(LineError::new(
            span,
            format!("unknown prefix `{name}:`; quote text that holds a colon"),
        )
        .expecting(SEGMENT_PREFIXES));
    }
    let rest = rest.ok_or_else(|| missing("a value"))?;
    let kind = match name {
        "testid" => SegmentKind::TestId(rest.into_value()?),
        "css" => SegmentKind::Css(rest.into_value()?),
        "frame" => SegmentKind::Frame(rest.into_value()?),
        "ai" => SegmentKind::Ai(rest.into_value()?),
        _ => {
            let Some(index) = rest.bare_single().and_then(parse_index) else {
                return Err(LineError::new(span, "expected an index after `nth:`")
                    .expecting(["a 0-based index like 0 or -1"]));
            };
            if is_first {
                return Err(LineError::new(
                    span,
                    "`nth:` may not be the first segment of a locator",
                )
                .expecting(["a locator segment before `nth:`"]));
            }
            SegmentKind::Nth(index)
        }
    };
    Ok(LocatorSegment { kind, span })
}

/// Splits the substring marker from the value of a role or text prefix
/// (SPEC 6.1): a bare `~` right after the colon, as in `button:~Sign`,
/// matches by substring. Returns `None` when nothing follows the `~`.
fn match_mode(rest: RawToken) -> Option<(bool, RawToken)> {
    match rest.parts.first() {
        Some(RawPart::Bare { text, .. }) if text.starts_with('~') => {
            strip_prefix_token(rest, 1).map(|rest| (true, rest))
        }
        _ => Some((false, rest)),
    }
}

/// Unprefixed text: a default-engine segment, which only actions allow
/// (SPEC 6.1). `>>` separates segments, so it is never one.
fn unprefixed_segment(token: RawToken, allow_default: bool) -> Result<LocatorSegment, LineError> {
    let span = token.span;
    if token.bare_single() == Some(">>") {
        return Err(
            LineError::new(span, "`>>` separates segments; quote it to match the text")
                .expecting(["a locator segment"]),
        );
    }
    if !allow_default {
        return Err(LineError::new(
            span,
            "unprefixed locator segments are only allowed in actions; use a prefix here",
        )
        .expecting(SEGMENT_PREFIXES));
    }
    Ok(LocatorSegment {
        kind: SegmentKind::Default(token.into_value()?),
        span,
    })
}

fn finish_locator(segments: Vec<LocatorSegment>, span: Span) -> Result<Locator, LineError> {
    if let Some(after) = segments
        .iter()
        .skip_while(|segment| !matches!(segment.kind, SegmentKind::Ai(_)))
        .nth(1)
    {
        return Err(LineError::new(
            after.span,
            "`ai:` must be the last segment of a locator",
        ));
    }
    if segments
        .iter()
        .rev()
        .find(|segment| !matches!(segment.kind, SegmentKind::Nth(_)))
        .is_some_and(|segment| matches!(segment.kind, SegmentKind::Frame(_)))
    {
        return Err(LineError::new(
            span,
            "a frame locator needs an element segment inside the frame",
        ));
    }
    Ok(Locator { segments, span })
}

fn locator_span(first: Span, last: Span) -> Span {
    Span {
        line:   first.line,
        column: first.column,
        len:    (last.column + last.len).saturating_sub(first.column),
    }
}

/// Takes a locator from the front of `tokens`: segments joined by `>>`
/// (SPEC 6). It ends at the first token after a segment that is not `>>`.
fn take_locator(
    tokens: &mut Peekable<IntoIter<RawToken>>,
    allow_default: bool,
    missing_at: Span,
) -> Result<Locator, LineError> {
    let Some(first) = tokens.next() else {
        return Err(LineError::new(missing_at, "expected a locator").expecting(["a locator"]));
    };
    let first_span = first.span;
    let mut last_span = first.span;
    let mut segments = vec![parse_segment(first, allow_default, true)?];
    while tokens
        .peek()
        .is_some_and(|token| token.bare_single() == Some(">>"))
    {
        let separator = tokens.next().expect("the peeked separator is present");
        let Some(next) = tokens.next() else {
            return Err(
                LineError::new(separator.span, "expected a locator segment after `>>`")
                    .expecting(["a locator segment"]),
            );
        };
        last_span = next.span;
        segments.push(parse_segment(next, allow_default, false)?);
    }
    finish_locator(segments, locator_span(first_span, last_span))
}

/// Builds a locator from all of `tokens`.
fn build_locator(
    tokens: Vec<RawToken>,
    allow_default: bool,
    missing_at: Span,
) -> Result<Locator, LineError> {
    let mut tokens = tokens.into_iter().peekable();
    let locator = take_locator(&mut tokens, allow_default, missing_at)?;
    if let Some(extra) = tokens.next() {
        return Err(
            LineError::new(extra.span, "expected `>>` between locator segments").expecting([">>"]),
        );
    }
    Ok(locator)
}

/// Strips the final step timeout, a bare `@duration` token (SPEC 12).
fn split_timeout(tokens: &mut Vec<RawToken>) -> Option<DurationLit> {
    let timeout = tokens.last()?.timeout?;
    tokens.pop();
    Some(timeout)
}

const ACTION_KEYWORDS: [&str; 29] = [
    "HTTP",
    "RESPONSE",
    "MOCK",
    "POPUP",
    "WINDOW",
    "CLOSE",
    "VISIT",
    "CLICK",
    "RIGHTCLICK",
    "MIDDLECLICK",
    "DBLCLICK",
    "FILL",
    "TYPE",
    "PRESS",
    "CHECK",
    "UNCHECK",
    "SELECT",
    "HOVER",
    "DRAG",
    "SCROLL",
    "UPLOAD",
    "DROP",
    "SCREENSHOT",
    "SNAPSHOT",
    "EVAL",
    "ACT",
    "GOAL",
    "EXTRACT",
    "STORE",
];

/// `SCREENSHOT`, `SNAPSHOT`, window, response, and extract names: an
/// identifier that may also contain hyphens (SPEC 7, 14).
fn is_artifact_name(text: &str) -> bool {
    let mut chars = text.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    (first.is_ascii_alphabetic() || first == '_')
        && chars.all(|ch| ch.is_ascii_alphanumeric() || ch == '_' || ch == '-')
}

/// Reads a name token.
fn name_token(token: &RawToken) -> Result<Ident, LineError> {
    match token.bare_single() {
        Some(text) if is_artifact_name(text) => Ok(Ident {
            text: text.to_owned(),
            span: token.span,
        }),
        _ => Err(LineError::new(token.span, "expected a name")
            .expecting(["a name matching [A-Za-z_][A-Za-z0-9_-]*"])),
    }
}

/// The tokens of an action line after its keyword and step timeout, read
/// left to right (SPEC 7).
struct Args {
    tokens: Peekable<IntoIter<RawToken>>,
    /// The span of the last token read; an error about a missing token
    /// points just after it.
    last:   Span,
}

impl Args {
    fn new(tokens: Vec<RawToken>, keyword_span: Span) -> Self {
        Self {
            tokens: tokens.into_iter().peekable(),
            last:   keyword_span,
        }
    }

    fn remaining(&self) -> usize {
        self.tokens.len()
    }

    fn peek(&mut self) -> Option<&RawToken> {
        self.tokens.peek()
    }

    /// The next token, or an error that names what should come next.
    fn next(&mut self, expected: &str) -> Result<RawToken, LineError> {
        let Some(token) = self.tokens.next() else {
            return Err(
                LineError::new(after_span(self.last), format!("expected {expected}"))
                    .expecting([expected]),
            );
        };
        self.last = token.span;
        Ok(token)
    }

    fn value(&mut self, expected: &str) -> Result<Value, LineError> {
        self.next(expected)?.into_value()
    }

    fn name(&mut self) -> Result<Ident, LineError> {
        name_token(&self.next("a name")?)
    }

    fn locator(&mut self, allow_default: bool) -> Result<Locator, LineError> {
        let locator = take_locator(&mut self.tokens, allow_default, after_span(self.last))?;
        self.last = locator.span;
        Ok(locator)
    }

    /// An uppercase HTTP method, such as `GET`.
    fn method(&mut self) -> Result<String, LineError> {
        let token = self.next("an HTTP method like GET or POST")?;
        token
            .bare_single()
            .filter(|text| text.chars().all(|ch| ch.is_ascii_uppercase()))
            .map(str::to_owned)
            .ok_or_else(|| {
                LineError::new(
                    token.span,
                    "expected an uppercase HTTP method like GET or POST",
                )
            })
    }

    /// Requires the line to end here.
    fn end(mut self) -> Result<(), LineError> {
        match self.tokens.next() {
            Some(extra) => Err(LineError::new(extra.span, "expected end of line")
                .expecting(["end of line", "@duration"])),
            None => Ok(()),
        }
    }
}

/// The one value of a line's remaining tokens.
fn only_value(tokens: Vec<RawToken>, span: Span) -> Result<Value, LineError> {
    let mut args = Args::new(tokens, span);
    let value = args.value("a value")?;
    args.end()?;
    Ok(value)
}

/// Parses `STORE scope key value` (SPEC 7): a bare storage scope, then a
/// key and a value.
fn parse_store(mut args: Args) -> Result<ActionKind, LineError> {
    let scope_expected = ["local", "session", "cookie"];
    let scope_token = args.next("a storage scope")?;
    let scope = match scope_token.bare_single() {
        Some("local") => StoreScope::Local,
        Some("session") => StoreScope::Session,
        Some("cookie") => StoreScope::Cookie,
        _ => {
            return Err(LineError::new(scope_token.span, "expected a storage scope")
                .expecting(scope_expected));
        }
    };
    let key = args.value("a key")?;
    let value = args.value("a value")?;
    args.end()?;
    Ok(ActionKind::Store { scope, key, value })
}

/// Parses `HTTP METHOD url` or `RESPONSE name METHOD url` (SPEC 7.2,
/// 7.3).
fn parse_network_action(keyword: &str, mut args: Args) -> Result<ActionKind, LineError> {
    let name = if keyword == "RESPONSE" {
        Some(args.name()?)
    } else {
        None
    };
    let method = args.method()?;
    let url = args.value("a URL")?;
    args.end()?;
    Ok(match name {
        Some(name) => ActionKind::Response { name, method, url },
        None => ActionKind::Http {
            method,
            url,
            headers: Vec::new(),
            body: None,
            source: String::new(),
        },
    })
}

fn response_name(token: RawToken) -> Result<Ident, LineError> {
    let span = token.span;
    let name = strip_prefix_token(token, "response:".len())
        .ok_or_else(|| LineError::new(span, "expected a response name"))?;
    name_token(&name)
}

/// Response field keywords, for diagnostics (SPEC 9.2).
const RESPONSE_FIELDS: [&str; 7] = [
    "status",
    "header:NAME",
    "location",
    "body",
    "bytes",
    "json:PATH",
    "xpath:EXPR",
];

/// True when a bare token starts a response field.
fn is_response_field(text: &str) -> bool {
    matches!(text, "status" | "location" | "body" | "bytes")
        || text.starts_with("header:")
        || text.starts_with("json:")
        || text.starts_with("xpath:")
}

const REQUEST_FIELDS: [&str; 7] = [
    "method",
    "url",
    "header:NAME",
    "body",
    "bytes",
    "json:PATH",
    "xpath:EXPR",
];

/// Parses the field after `request:NAME` (SPEC 9.2).
fn parse_request_field(cursor: &mut Cursor, span: Span) -> Result<RequestField, LineError> {
    let token = cursor.next_token()?.ok_or_else(|| {
        LineError::new(after_span(span), "expected a request field").expecting(REQUEST_FIELDS)
    })?;
    match token.bare_single() {
        Some("method") => return Ok(RequestField::Method),
        Some("url") => return Ok(RequestField::Url),
        Some("body") => return Ok(RequestField::Body),
        Some("bytes") => return Ok(RequestField::Bytes),
        _ => {}
    }
    let head = match token.parts.first() {
        Some(RawPart::Bare { text, .. }) => text.as_str(),
        _ => "",
    };
    if head.starts_with("json:") {
        return Ok(RequestField::Json(json_path_value(token)?));
    }
    if head.starts_with("xpath:") {
        return Ok(RequestField::Xpath(xpath_value(token)?));
    }
    if !head.starts_with("header:") {
        return Err(
            LineError::new(token.span, "expected a request field").expecting(REQUEST_FIELDS)
        );
    }
    let field_span = token.span;
    let value = strip_prefix_token(token, "header:".len())
        .ok_or_else(|| LineError::new(field_span, "expected a header name after `header:`"))?
        .into_value()?;
    if let Some(literal) = value.as_literal()
        && !is_attr_name(&literal)
    {
        return Err(LineError::new(field_span, "invalid request header name"));
    }
    Ok(RequestField::Header(value))
}

fn parse_response_field(cursor: &mut Cursor, span: Span) -> Result<ResponseField, LineError> {
    let token = cursor.next_token()?.ok_or_else(|| {
        LineError::new(after_span(span), "expected a response field").expecting(RESPONSE_FIELDS)
    })?;
    parse_response_field_token(token)
}

fn parse_response_field_token(token: RawToken) -> Result<ResponseField, LineError> {
    match token.bare_single() {
        Some("status") => return Ok(ResponseField::Status),
        Some("location") => return Ok(ResponseField::Location),
        Some("body") => return Ok(ResponseField::Body),
        Some("bytes") => return Ok(ResponseField::Bytes),
        _ => {}
    }
    let head = match token.parts.first() {
        Some(RawPart::Bare { text, .. }) => text.as_str(),
        _ => "",
    };
    if head.starts_with("json:") {
        return Ok(ResponseField::Json(json_path_value(token)?));
    }
    if head.starts_with("xpath:") {
        return Ok(ResponseField::Xpath(xpath_value(token)?));
    }
    if !head.starts_with("header:") {
        return Err(
            LineError::new(token.span, "expected a response field").expecting(RESPONSE_FIELDS)
        );
    }
    let field_span = token.span;
    let value = strip_prefix_token(token, "header:".len())
        .ok_or_else(|| LineError::new(field_span, "expected a header name after `header:`"))?
        .into_value()?;
    if let Some(literal) = value.as_literal()
        && !is_attr_name(&literal)
    {
        return Err(LineError::new(field_span, "invalid response header name"));
    }
    Ok(ResponseField::Header(value))
}

/// The argument of a `json:` or `xpath:` token (SPEC 9.5), which joins a
/// quoted string like any other prefix value.
fn prefixed_value(token: RawToken, prefix: &str) -> Result<Value, LineError> {
    let span = token.span;
    strip_prefix_token(token, prefix.len())
        .ok_or_else(|| {
            LineError::new(span, format!("`{prefix}` needs an argument")).expecting(["a query"])
        })?
        .into_value()
}

/// A `json:PATH` argument, with a literal path checked here (SPEC 9.5).
fn json_path_value(token: RawToken) -> Result<Value, LineError> {
    let span = token.span;
    let value = prefixed_value(token, "json:")?;
    if let Some(literal) = value.as_literal() {
        JsonQuery::parse(&literal).map_err(|message| LineError::new(span, message))?;
    }
    Ok(value)
}

/// An `xpath:EXPR` argument, with a literal expression checked here (SPEC
/// 9.5).
fn xpath_value(token: RawToken) -> Result<Value, LineError> {
    let span = token.span;
    let value = prefixed_value(token, "xpath:")?;
    if let Some(literal) = value.as_literal() {
        XpathQuery::parse(&literal).map_err(|message| LineError::new(span, message))?;
    }
    Ok(value)
}

/// Parses `MOCK METHOD url STATUS` or `MOCK METHOD url failed` (SPEC
/// 7.5). Header and body lines follow in [`Parser::parse_http_tail`].
fn parse_mock(mut args: Args) -> Result<ActionKind, LineError> {
    let method = args.method()?;
    let url = args.value("a URL")?;
    let answer = args.next("a status code from 200 to 599, or `failed`")?;
    let response = match answer.bare_single() {
        Some("failed") => MockResponse::Failed,
        Some(text)
            if text.len() == 3
                && text.bytes().all(|byte| byte.is_ascii_digit())
                && (200..=599).contains(&text.parse::<u16>().unwrap_or(0)) =>
        {
            MockResponse::Fulfill {
                status:  text.parse().expect("three digits parse as a u16"),
                headers: Vec::new(),
                body:    None,
            }
        }
        _ => {
            return Err(LineError::new(
                answer.span,
                "expected a status code from 200 to 599, or `failed`",
            )
            .expecting(["a status code", "failed"]));
        }
    };
    args.end()?;
    Ok(ActionKind::Mock {
        method,
        url,
        response,
        source: String::new(),
    })
}

/// Parses an action line after its keyword (SPEC 7, 17), left to right.
fn parse_action_body(
    keyword: &str,
    keyword_span: Span,
    cursor: &mut Cursor,
) -> Result<(ActionKind, Option<DurationLit>), LineError> {
    let mut tokens = Vec::new();
    while let Some(token) = cursor.next_token()? {
        tokens.push(token);
    }
    if keyword == "MOCK" {
        // A mock registers at once (SPEC 7.5).
        if let Some(last) = tokens.last().filter(|last| last.timeout.is_some()) {
            return Err(LineError::new(last.span, "MOCK has no step timeout"));
        }
        return Ok((parse_mock(Args::new(tokens, keyword_span))?, None));
    }
    let timeout = split_timeout(&mut tokens);
    let mut args = Args::new(tokens, keyword_span);
    let kind = match keyword {
        "HTTP" | "RESPONSE" => return Ok((parse_network_action(keyword, args)?, timeout)),
        "STORE" => return Ok((parse_store(args)?, timeout)),
        "SCROLL" => return Ok((parse_scroll(args)?, timeout)),
        "POPUP" => ActionKind::Popup { name: args.name()? },
        "WINDOW" => ActionKind::Window { name: args.name()? },
        "CLOSE" => ActionKind::Close { name: args.name()? },
        "SCREENSHOT" => ActionKind::Screenshot { name: args.name()? },
        "VISIT" => ActionKind::Visit {
            url: args.value("a URL")?,
        },
        "EVAL" => ActionKind::Eval {
            script: args.value("a script")?,
        },
        "GOAL" => ActionKind::Goal {
            goal: args.value("a goal")?,
        },
        "CLICK" | "RIGHTCLICK" | "MIDDLECLICK" => ActionKind::Click {
            target: args.locator(true)?,
            button: match keyword {
                "RIGHTCLICK" => MouseButton::Right,
                "MIDDLECLICK" => MouseButton::Middle,
                _ => MouseButton::Left,
            },
        },
        "DBLCLICK" => ActionKind::Dblclick {
            target: args.locator(true)?,
        },
        "HOVER" => ActionKind::Hover {
            target: args.locator(true)?,
        },
        "CHECK" => ActionKind::Check {
            target: args.locator(true)?,
        },
        "UNCHECK" => ActionKind::Uncheck {
            target: args.locator(true)?,
        },
        "FILL" => ActionKind::Fill {
            target: args.locator(true)?,
            value:  args.value("a value")?,
        },
        "TYPE" => ActionKind::Type {
            target: args.locator(true)?,
            text:   args.value("the text to type")?,
        },
        "SELECT" => ActionKind::Select {
            target: args.locator(true)?,
            option: args.value("an option label")?,
        },
        // With one value, PRESS takes it as the key (SPEC 7).
        "PRESS" if args.remaining() > 1 => ActionKind::Press {
            target: Some(args.locator(true)?),
            key:    args.value("a key")?,
        },
        "PRESS" => ActionKind::Press {
            target: None,
            key:    args.value("a key like Enter")?,
        },
        "DRAG" => {
            let source = args.locator(true)?;
            let to = args.next("`to` and the drop target")?;
            if to.bare_single() != Some("to") {
                return Err(LineError::new(
                    to.span,
                    "expected `to` between the element to drag and its target",
                )
                .expecting(["to", ">>"]));
            }
            ActionKind::Drag {
                source,
                target: args.locator(true)?,
            }
        }
        "UPLOAD" | "DROP" => {
            let target = args.locator(true)?;
            let path = file_path(args.next("a `file:` path")?)?;
            if keyword == "UPLOAD" {
                ActionKind::Upload { target, path }
            } else {
                ActionKind::Drop { target, path }
            }
        }
        "SNAPSHOT" => {
            let name = args.name()?;
            let target = if args.remaining() > 0 {
                Some(args.locator(false)?)
            } else {
                None
            };
            ActionKind::Snapshot {
                name,
                target,
                options: Vec::new(),
            }
        }
        "ACT" => {
            let (scope, instruction) = parse_act(args)?;
            return Ok((ActionKind::Act { scope, instruction }, timeout));
        }
        "EXTRACT" => {
            let name = args.name()?;
            let (scope, instruction) = parse_act(args)?;
            return Ok((
                ActionKind::Extract {
                    name,
                    scope,
                    instruction,
                    schema: None,
                },
                timeout,
            ));
        }
        other => {
            return Err(
                LineError::new(keyword_span, format!("unknown action `{other}`"))
                    .expecting(ACTION_KEYWORDS),
            );
        }
    };
    args.end()?;
    Ok((kind, timeout))
}

/// The scope and instruction of `ACT`, `EXTRACT`, and `JUDGE` (SPEC 7.4):
/// a first token with a prefix starts the scope, and the instruction
/// follows it.
fn parse_act(mut args: Args) -> Result<(Option<Locator>, Value), LineError> {
    let scoped = args
        .peek()
        .is_some_and(|token| segment_prefix(token).is_some());
    let scope = if scoped {
        Some(args.locator(false)?)
    } else {
        None
    };
    let instruction = args.value("an instruction")?;
    args.end()?;
    Ok((scope, instruction))
}

/// `SCROLL motion`, or `SCROLL locator [motion]` (SPEC 7). A bare
/// direction or `to` right after `SCROLL` is always the motion.
fn parse_scroll(mut args: Args) -> Result<ActionKind, LineError> {
    if args.remaining() == 0 {
        return Err(
            LineError::new(after_span(args.last), "expected what to scroll").expecting([
                "a locator",
                "down",
                "up",
                "left",
                "right",
                "to",
            ]),
        );
    }
    if let Some(motion) = scroll_motion(&mut args)? {
        args.end()?;
        return Ok(ActionKind::Scroll {
            target: None,
            motion,
        });
    }
    let target = args.locator(true)?;
    let kind = match scroll_motion(&mut args)? {
        Some(motion) => ActionKind::Scroll {
            target: Some(target),
            motion,
        },
        None => ActionKind::ScrollIntoView { target },
    };
    args.end()?;
    Ok(kind)
}

/// Reads a scroll motion, when the next token starts one: a bare
/// direction, or a bare `to` and a percent.
fn scroll_motion(args: &mut Args) -> Result<Option<ScrollMotion>, LineError> {
    let word = args.peek().and_then(RawToken::bare_single);
    if let Some(direction) = word.and_then(ScrollDirection::from_keyword) {
        args.next("a direction")?;
        return Ok(Some(ScrollMotion::Chunk(direction)));
    }
    if word != Some("to") {
        return Ok(None);
    }
    args.next("`to`")?;
    let percent = args.next("a percent such as 50%")?;
    let Some(percent) = percent.bare_single().and_then(Percent::parse) else {
        return Err(LineError::new(
            percent.span,
            "expected a percent from 0% to 100% after `to`",
        )
        .expecting(["a percent such as 50%"]));
    };
    Ok(Some(ScrollMotion::To(percent)))
}

/// The `file:path` value of `UPLOAD` and `DROP` (SPEC 7). A quoted
/// `"file:..."` is a value, not the prefix.
fn file_path(token: RawToken) -> Result<Value, LineError> {
    let span = token.span;
    let prefixed = matches!(
        token.parts.first(),
        Some(RawPart::Bare { text, .. }) if text.starts_with("file:")
    );
    if !prefixed {
        return Err(LineError::new(span, "expected a `file:` path").expecting(["file:"]));
    }
    strip_prefix_token(token, "file:".len())
        .ok_or_else(|| LineError::new(span, "expected a path after `file:`").expecting(["a path"]))?
        .into_value()
}

/// The position just after a span, for "expected more here" diagnostics.
fn after_span(span: Span) -> Span {
    Span {
        line:   span.line,
        column: span.column + span.len + 1,
        len:    1,
    }
}

/// Scans the locator of an `ASSERT` or `CAPTURE` line (SPEC 9.2):
/// segments joined by `>>`, each one token. Returns the locator and the
/// token after it, or `None` when the line ended first.
fn scan_locator(
    first: RawToken,
    cursor: &mut Cursor,
) -> Result<(Locator, Option<RawToken>), LineError> {
    let first_span = first.span;
    let mut last_span = first.span;
    let mut segments = vec![parse_segment(first, false, true)?];
    loop {
        let span = locator_span(first_span, last_span);
        let token = cursor.next_token()?;
        if token.as_ref().and_then(RawToken::bare_single) != Some(">>") {
            return Ok((finish_locator(segments, span)?, token));
        }
        let separator = token.expect("a separator token is present");
        let Some(next) = cursor.next_token()? else {
            return Err(
                LineError::new(separator.span, "expected a locator segment after `>>`")
                    .expecting(["a locator segment"]),
            );
        };
        last_span = next.span;
        segments.push(parse_segment(next, false, false)?);
    }
}

const STATE_CHECKS: [(&str, StateCheck); 7] = [
    ("visible", StateCheck::Visible),
    ("hidden", StateCheck::Hidden),
    ("enabled", StateCheck::Enabled),
    ("disabled", StateCheck::Disabled),
    ("checked", StateCheck::Checked),
    ("unchecked", StateCheck::Unchecked),
    ("focused", StateCheck::Focused),
];

const ASSERT_CHECKS: [&str; 11] = [
    "visible",
    "hidden",
    "enabled",
    "disabled",
    "checked",
    "unchecked",
    "focused",
    "text",
    "value",
    "attr:NAME",
    "count",
];

const EXTRACTORS: [&str; 4] = ["text", "value", "count", "attr:NAME"];

fn is_extractor(text: &str) -> bool {
    matches!(text, "text" | "value" | "count") || text.starts_with("attr:")
}

fn is_assert_stop(text: &str) -> bool {
    STATE_CHECKS.iter().any(|(name, _)| *name == text) || is_extractor(text)
}

/// Parses an operand: the next token as a value. A final bare `@duration`
/// is the step timeout, never the operand (SPEC 3.1).
fn parse_operand(cursor: &mut Cursor, op_span: Span) -> Result<Value, LineError> {
    let Some(token) = cursor.next_token()? else {
        return Err(LineError::new(after_span(op_span), "expected a value").expecting(["a value"]));
    };
    if token.timeout.is_some() && cursor.at_line_end() {
        return Err(
            LineError::new(token.span, "expected a value before the step timeout")
                .expecting(["a value"]),
        );
    }
    token.into_value()
}

/// Parses an expected value: a single-line JSON literal when the next
/// character starts one, else a value (SPEC 3.1). `{{` starts a variable
/// reference, not a JSON object.
fn parse_expected(cursor: &mut Cursor, op_span: Span) -> Result<Operand, LineError> {
    cursor.skip_ws();
    let starts_json = match cursor.peek() {
        Some('[') => true,
        Some('{') => cursor.chars.get(cursor.pos + 1) != Some(&'{'),
        _ => false,
    };
    if starts_json {
        return cursor.scan_json_literal().map(Operand::Json);
    }
    parse_operand(cursor, op_span).map(Operand::Value)
}

/// Checks a regex literal in Unicode mode (SPEC 3.1).
fn validate_regex(regex: &Regex) -> Result<(), LineError> {
    let flags = PatternFlags {
        ignore_case: regex.flags.ignore_case,
        dot_all:     regex.flags.dot_all,
        multiline:   regex.flags.multiline,
    };
    Pattern::validate(&regex.pattern, flags).map_err(|error| {
        LineError::new(
            regex.span,
            format!("invalid regex in Unicode mode: {}", error.reason),
        )
    })
}

/// Parses an index: an integer with an optional minus sign (SPEC 3.1).
fn parse_index(text: &str) -> Option<i64> {
    let digits = text.strip_prefix('-').unwrap_or(text);
    if digits.is_empty() || !digits.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    text.parse().ok()
}

/// Parses filters until a token that is not a filter; returns the
/// filters and that token (SPEC 9.5).
fn parse_filters(cursor: &mut Cursor) -> Result<(Vec<FilterSpec>, Option<RawToken>), LineError> {
    let mut filters = Vec::new();
    loop {
        let Some(token) = cursor.next_token()? else {
            return Ok((filters, None));
        };
        let keyword = token.bare_single().and_then(|text| {
            FILTER_KEYWORDS
                .iter()
                .find(|(name, _)| *name == text)
                .map(|(_, kind)| *kind)
        });
        let head = match token.parts.first() {
            Some(RawPart::Bare { text, .. }) => text.as_str(),
            _ => "",
        };
        let (head_is_json, head_is_xpath) = (head.starts_with("json:"), head.starts_with("xpath:"));
        let span = token.span;
        let (kind, args) = if head_is_json {
            (FilterKind::Json, vec![FilterArg::Value(json_path_value(
                token,
            )?)])
        } else if head_is_xpath {
            (FilterKind::Xpath, vec![FilterArg::Value(xpath_value(
                token,
            )?)])
        } else if let Some(kind) = keyword {
            (kind, parse_filter_args(kind, cursor, span)?)
        } else {
            return Ok((filters, Some(token)));
        };
        filters.push(FilterSpec { kind, args, span });
    }
}

/// Parses the arguments of one filter keyword (SPEC 9.5), checking
/// literal regexes, date formats, and charset labels.
fn parse_filter_args(
    kind: FilterKind,
    cursor: &mut Cursor,
    span: Span,
) -> Result<Vec<FilterArg>, LineError> {
    let value = |cursor: &mut Cursor| parse_operand(cursor, span).map(FilterArg::Value);
    let regex = |cursor: &mut Cursor| -> Result<FilterArg, LineError> {
        let regex = cursor.expect_regex()?;
        validate_regex(&regex)?;
        Ok(FilterArg::Regex(regex))
    };
    Ok(match kind {
        FilterKind::Nth => {
            let token = cursor.next_token()?.ok_or_else(|| {
                LineError::new(after_span(span), "expected an index after `nth`")
                    .expecting(["an index like 0 or -1"])
            })?;
            let index = token.bare_single().and_then(parse_index).ok_or_else(|| {
                LineError::new(token.span, "expected an index after `nth`")
                    .expecting(["an index like 0 or -1"])
            })?;
            vec![FilterArg::Index(index)]
        }
        FilterKind::Split | FilterKind::UrlQueryParam => vec![value(cursor)?],
        FilterKind::ToDate | FilterKind::DateFormat => {
            let format = parse_operand(cursor, span)?;
            if let Some(literal) = format.as_literal() {
                DateFormat::new(&literal)
                    .map_err(|message| LineError::new(format.span, message))?;
            }
            vec![FilterArg::Value(format)]
        }
        FilterKind::CharsetDecode => {
            let label = parse_operand(cursor, span)?;
            if let Some(literal) = label.as_literal() {
                Charset::from_label(&literal)
                    .map_err(|message| LineError::new(label.span, message))?;
            }
            vec![FilterArg::Value(label)]
        }
        FilterKind::Replace => vec![value(cursor)?, value(cursor)?],
        FilterKind::Regex => vec![regex(cursor)?],
        FilterKind::ReplaceRegex => vec![regex(cursor)?, value(cursor)?],
        _ => Vec::new(),
    })
}

/// Every predicate spelling, for diagnostics.
fn predicate_keywords() -> Vec<&'static str> {
    let mut names: Vec<&str> = COMPARE_KEYWORDS.iter().map(|(name, _)| *name).collect();
    names.push("matches");
    names.extend(WORD_PREDICATES.iter().map(|(name, _)| *name));
    names
}

/// Parses `[not] predicate` starting at `first` (SPEC 9.4).
fn parse_predicate(
    first: RawToken,
    cursor: &mut Cursor,
) -> Result<(bool, PredicateSpec), LineError> {
    let (negated, token) = if first.bare_single() == Some("not") {
        let next = cursor.next_token()?.ok_or_else(|| {
            LineError::new(after_span(first.span), "expected a predicate after `not`")
                .expecting(predicate_keywords())
        })?;
        (true, next)
    } else {
        (false, first)
    };
    let text = token.bare_single().unwrap_or_default();
    if text == "matches" {
        let regex = cursor.expect_regex()?;
        validate_regex(&regex)?;
        return Ok((negated, PredicateSpec::Matches(regex)));
    }
    if let Some((_, kind)) = COMPARE_KEYWORDS.iter().find(|(name, _)| *name == text) {
        let expected = parse_expected(cursor, token.span)?;
        return Ok((negated, PredicateSpec::Compare {
            kind: *kind,
            expected,
        }));
    }
    if let Some((_, kind)) = WORD_PREDICATES.iter().find(|(name, _)| *name == text) {
        return Ok((negated, PredicateSpec::Word(*kind)));
    }
    Err(
        LineError::new(token.span, "expected a filter or a predicate")
            .expecting(predicate_keywords()),
    )
}

/// Consumes an optional final `@duration` and requires the line to end.
fn parse_line_timeout(cursor: &mut Cursor) -> Result<Option<DurationLit>, LineError> {
    let Some(token) = cursor.next_token()? else {
        return Ok(None);
    };
    let mut tokens = vec![token];
    let timeout = split_timeout(&mut tokens);
    if let Some(extra) = tokens.first() {
        return Err(LineError::new(extra.span, "expected end of line")
            .expecting(["@duration", "end of line"]));
    }
    if let Some(extra) = cursor.next_token()? {
        return Err(LineError::new(extra.span, "expected end of line").expecting(["end of line"]));
    }
    Ok(timeout)
}

/// What an `ASSERT` or `CAPTURE` line starts with.
enum Head {
    Subject(Subject),
    State {
        locator: Locator,
        state:   StateCheck,
    },
}

/// Parses a subject (SPEC 9.2). In an HTTP entry only the entry's own
/// response fields are allowed. State checks end the line only in
/// `ASSERT` lines.
fn parse_subject(
    first: RawToken,
    cursor: &mut Cursor,
    implicit_http: bool,
    in_asserts: bool,
) -> Result<Head, LineError> {
    let first_span = first.span;
    let text = first.bare_single().map(str::to_owned);
    let head_text = match first.parts.first() {
        Some(RawPart::Bare { text, .. }) => text.clone(),
        _ => String::new(),
    };
    if implicit_http {
        if !is_response_field(&head_text) {
            let what = if in_asserts { "check" } else { "capture" };
            return Err(LineError::new(
                first_span,
                format!("an HTTP entry can only {what} its response"),
            )
            .expecting(RESPONSE_FIELDS));
        }
        let field = parse_response_field_token(first)?;
        return Ok(Head::Subject(Subject::Response { name: None, field }));
    }
    if head_text.starts_with("extract:") {
        let span = first.span;
        let name = strip_prefix_token(first, "extract:".len())
            .ok_or_else(|| LineError::new(span, "expected an EXTRACT name"))?;
        let name = name_token(&name)?;
        return Ok(Head::Subject(Subject::Extract { name, text: false }));
    }
    if head_text.starts_with("request:") {
        let span = first.span;
        let name = strip_prefix_token(first, "request:".len())
            .ok_or_else(|| LineError::new(span, "expected a response name"))?;
        let name = name_token(&name)?;
        let field = parse_request_field(cursor, first_span)?;
        return Ok(Head::Subject(Subject::Request { name, field }));
    }
    if head_text.starts_with("response:") {
        let name = response_name(first)?;
        let field = parse_response_field(cursor, first_span)?;
        return Ok(Head::Subject(Subject::Response {
            name: Some(name),
            field,
        }));
    }
    match text.as_deref() {
        Some("url") => return Ok(Head::Subject(Subject::Url)),
        Some("title") => return Ok(Head::Subject(Subject::Title)),
        Some("eval") => {
            let script = parse_operand(cursor, first_span)?;
            return Ok(Head::Subject(Subject::Eval(script)));
        }
        _ => {}
    }
    let (is_stop, expected): (fn(&str) -> bool, &[&str]) = if in_asserts {
        (is_assert_stop, &ASSERT_CHECKS)
    } else {
        (is_extractor, &EXTRACTORS)
    };
    let (locator, stop) = scan_locator(first, cursor)?;
    if let Some(token) = stop
        .as_ref()
        .filter(|token| !token.bare_single().is_some_and(is_stop))
    {
        let mut alternatives = vec![">>"];
        alternatives.extend_from_slice(expected);
        return Err(
            LineError::new(token.span, "expected `>>` or the end of the locator")
                .expecting(alternatives),
        );
    }
    let Some(stop) = stop else {
        let what = if in_asserts {
            "a check"
        } else {
            "an extractor"
        };
        return Err(
            LineError::new(after_span(locator.span), format!("expected {what}"))
                .expecting(expected.iter().copied()),
        );
    };
    let stop_text = stop.bare_single().unwrap_or_default();
    if let Some((_, state)) = STATE_CHECKS.iter().find(|(name, _)| *name == stop_text) {
        return Ok(Head::State {
            locator,
            state: *state,
        });
    }
    let extractor = match stop_text {
        "text" => Extractor::Text,
        "value" => Extractor::Value,
        "count" => Extractor::Count,
        other => {
            let attr = other.strip_prefix("attr:").unwrap_or_default();
            if !is_attr_name(attr) {
                return Err(LineError::new(stop.span, "invalid attribute name")
                    .expecting(["a name like aria-expanded or data-state"]));
            }
            Extractor::Attr(attr.to_owned())
        }
    };
    Ok(Head::Subject(Subject::Element { locator, extractor }))
}

/// Parses one `ASSERT` line after its keyword (SPEC 9, 17).
fn parse_assert_body(
    first: RawToken,
    cursor: &mut Cursor,
    implicit_http: bool,
) -> Result<(AssertBody, Option<DurationLit>), LineError> {
    let first_span = first.span;
    let is_window = !implicit_http
        && matches!(first.parts.first(), Some(RawPart::Bare { text, .. }) if text.starts_with("window:"));
    let body = if is_window {
        let rest = strip_prefix_token(first, "window:".len())
            .ok_or_else(|| LineError::new(first_span, "expected a window name"))?;
        let name = name_token(&rest)?;
        let check = cursor
            .next_token()?
            .ok_or_else(|| LineError::new(after_span(first_span), "expected `closed`"))?;
        if check.bare_single() != Some("closed") {
            return Err(LineError::new(check.span, "expected `closed`"));
        }
        AssertBody::WindowClosed { name }
    } else {
        match parse_subject(first, cursor, implicit_http, true)? {
            Head::State { locator, state } => AssertBody::ElementState { locator, state },
            Head::Subject(subject) => {
                let (filters, next) = parse_filters(cursor)?;
                let Some(next) = next else {
                    return Err(LineError::new(
                        after_span(filters.last().map_or(first_span, |filter| filter.span)),
                        "expected a predicate",
                    )
                    .expecting(predicate_keywords()));
                };
                let (negated, predicate) = parse_predicate(next, cursor)?;
                check_bytes_literal(&subject, &filters, &predicate)?;
                AssertBody::Check(CheckLine {
                    subject,
                    filters,
                    negated,
                    predicate,
                })
            }
        }
    };
    let timeout = parse_line_timeout(cursor)?;
    Ok((body, timeout))
}

/// In a typed check, a bare value shaped like a bytes literal must decode
/// (SPEC 9.6). In a text check it is plain text.
fn check_bytes_literal(
    subject: &Subject,
    filters: &[FilterSpec],
    predicate: &PredicateSpec,
) -> Result<(), LineError> {
    let PredicateSpec::Compare {
        expected: Operand::Value(value),
        ..
    } = predicate
    else {
        return Ok(());
    };
    if value.quoted || chain_type(subject, filters).is_ok_and(|value_type| value_type.is_string()) {
        return Ok(());
    }
    match value.as_literal() {
        Some(literal) if is_bytes_literal_shape(&literal) && bytes_literal(&literal).is_none() => {
            Err(LineError::new(
                value.span,
                format!("invalid bytes literal {}", quote_json(&literal)),
            ))
        }
        _ => Ok(()),
    }
}

/// Parses a `PAGE` line after its keyword (SPEC 8).
fn parse_page_body(
    keyword_span: Span,
    cursor: &mut Cursor,
) -> Result<(PageCheck, Option<DurationLit>), LineError> {
    let Some(token) = cursor.next_token()? else {
        return Err(LineError::new(after_span(keyword_span), "expected a value")
            .expecting(["a value", "matches /re/"]));
    };
    let check = if token.bare_single() == Some("matches") {
        let regex = cursor.expect_regex()?;
        validate_regex(&regex)?;
        PageCheck::Matches(regex)
    } else {
        if token.timeout.is_some() && cursor.at_line_end() {
            return Err(
                LineError::new(token.span, "expected a value before the step timeout")
                    .expecting(["a value", "matches /re/"]),
            );
        }
        PageCheck::Value(token.into_value()?)
    };
    let timeout = parse_line_timeout(cursor)?;
    Ok((check, timeout))
}

/// Reads the head of a `name: value` line (SPEC 3.1): the text before
/// the token's first colon, and its span. White space must follow the
/// colon, so `base:x` is an error. `None` when the token holds no colon.
fn colon_head(token: &RawToken) -> Result<Option<(String, Span)>, LineError> {
    let Some(RawPart::Bare { text, .. }) = token.parts.first() else {
        return Ok(None);
    };
    let Some(colon) = text.find(':') else {
        return Ok(None);
    };
    let name = text[..colon].to_owned();
    if colon + 1 < text.len() || token.parts.len() > 1 {
        return Err(
            LineError::new(token.span, format!("put a space after `{name}:`"))
                .expecting(["white space"]),
        );
    }
    let span = Span {
        line:   token.span.line,
        column: token.span.column,
        len:    u32::try_from(name.chars().count()).unwrap_or(u32::MAX),
    };
    Ok(Some((name, span)))
}

/// Reads the `name:` head of a `CAPTURE` line (SPEC 10).
fn capture_name(token: Option<RawToken>, keyword_span: Span) -> Result<Ident, LineError> {
    let expected = || {
        LineError::new(after_span(keyword_span), "expected a capture name")
            .expecting(["name: source"])
    };
    let token = token.ok_or_else(expected)?;
    match colon_head(&token)? {
        Some((text, span)) if is_ident(&text) => Ok(Ident { text, span }),
        _ => Err(LineError::new(token.span, "expected a capture name").expecting(["name: source"])),
    }
}

/// Parses one `CAPTURE` line after its `name:` head (SPEC 10, 17).
fn parse_capture_body(
    name: Ident,
    cursor: &mut Cursor,
    implicit_http: bool,
) -> Result<Capture, LineError> {
    let first = cursor.next_token()?.ok_or_else(|| {
        LineError::new(after_span(name.span), "expected a capture source").expecting([
            "a locator",
            "url",
            "title",
            "eval",
            "response:NAME",
        ])
    })?;
    let Head::Subject(subject) = parse_subject(first, cursor, implicit_http, false)? else {
        unreachable!("state checks only end ASSERT lines");
    };
    let (filters, next) = parse_filters(cursor)?;
    let mut timeout = None;
    if let Some(token) = next {
        let mut tokens = vec![token];
        timeout = split_timeout(&mut tokens);
        if let Some(extra) = tokens.first() {
            return Err(
                LineError::new(extra.span, "expected a filter or end of line").expecting([
                    "a filter",
                    "@duration",
                    "end of line",
                ]),
            );
        }
        if let Some(extra) = cursor.next_token()? {
            return Err(
                LineError::new(extra.span, "expected end of line").expecting(["end of line"])
            );
        }
    }
    Ok(Capture {
        name,
        subject,
        filters,
        timeout,
        line: 0,
        span: Span {
            line:   0,
            column: 0,
            len:    0,
        },
        text: String::new(),
    })
}

fn parse_snapshot_option(
    first: &RawToken,
    cursor: &mut Cursor,
) -> Result<SnapshotOption, LineError> {
    let span = first.span;
    let Some((key, _)) = colon_head(first)? else {
        return Err(LineError::new(span, "expected a snapshot option"));
    };
    let mut tokens = Vec::new();
    while let Some(token) = cursor.next_token()? {
        tokens.push(token);
    }
    if split_timeout(&mut tokens).is_some() {
        return Err(LineError::new(
            span,
            "put the timeout on the SNAPSHOT headline",
        ));
    }
    match key.as_str() {
        "snapshot-mask" => {
            if tokens.len() == 1 && tokens[0].bare_single() == Some("none") {
                Ok(SnapshotOption::Mask(None))
            } else {
                let locator = build_locator(tokens, false, after_span(span))?;
                if let Some(segment) = locator.segments.last()
                    && matches!(segment.kind, SegmentKind::Ai(_))
                {
                    // A mask can match many elements; `ai:` names one
                    // (SPEC 6.3).
                    return Err(LineError::new(
                        segment.span,
                        "`ai:` cannot be a snapshot mask",
                    ));
                }
                Ok(SnapshotOption::Mask(Some(locator)))
            }
        }
        "snapshot-max-diff" => Ok(SnapshotOption::MaxDiff(option_shape(
            only_value(tokens, span)?,
            |text| text.parse::<MaxDiff>().ok(),
            &["a pixel count or percentage from 0% to 100%"],
        )?)),
        "snapshot-pixel-threshold" => Ok(SnapshotOption::PixelThreshold(option_shape(
            only_value(tokens, span)?,
            |text| text.parse::<PixelThreshold>().ok(),
            &["a JSON number from 0 to 1"],
        )?)),
        _ => Err(
            LineError::new(span, format!("unknown snapshot option `{key}`")).expecting([
                "snapshot-mask",
                "snapshot-max-diff",
                "snapshot-pixel-threshold",
            ]),
        ),
    }
}

fn validate_snapshot_option<'a>(
    existing: impl Iterator<Item = &'a SnapshotOption>,
    new: &SnapshotOption,
    span: Span,
) -> Result<(), LineError> {
    for old in existing.filter(|old| old.key() == new.key()) {
        if matches!(
            (old, new),
            (SnapshotOption::Mask(Some(_)), SnapshotOption::Mask(Some(_)))
        ) {
            continue;
        }
        return Err(LineError::new(
            span,
            format!("duplicate or conflicting {} option", new.key()),
        ));
    }
    Ok(())
}

const OPTION_KEYS: [&str; 16] = [
    "base",
    "browser",
    "viewport",
    "step-timeout",
    "entry-timeout",
    "nav-timeout",
    "allow-hosts",
    "dialogs",
    "reduced-motion",
    "storage",
    "user-agent",
    "setup",
    "model",
    "snapshot-mask",
    "snapshot-max-diff",
    "snapshot-pixel-threshold",
];

/// Shape-validates a literal option value at parse time; a value with
/// interpolation is validated by the runner after resolution (SPEC 5, 11).
fn option_shape<T>(
    value: Value,
    parse: impl Fn(&str) -> Option<T>,
    expected: &[&str],
) -> Result<OptionValue<T>, LineError> {
    match value.as_literal() {
        Some(text) => match parse(&text) {
            Some(parsed) => Ok(OptionValue::Literal(parsed)),
            None => Err(
                LineError::new(value.span, format!("invalid option value `{text}`"))
                    .expecting(expected),
            ),
        },
        None => Ok(OptionValue::Interpolated(value)),
    }
}

/// Parses one `key: value` line in `[Options]` (SPEC 5).
fn parse_option_line(first: &RawToken, cursor: &mut Cursor) -> Result<FileOption, LineError> {
    let first_span = first.span;
    let Some((key, key_span)) = colon_head(first)? else {
        return Err(LineError::new(first_span, "expected an option line").expecting(["key: value"]));
    };
    if !OPTION_KEYS.contains(&key.as_str()) {
        return Err(
            LineError::new(key_span, format!("unknown option key `{key}`")).expecting(OPTION_KEYS),
        );
    }
    if key.starts_with("snapshot-") {
        return parse_snapshot_option(first, cursor).map(FileOption::Snapshot);
    }
    let mut values = Vec::new();
    while let Some(token) = cursor.next_token()? {
        values.push(token.into_value()?);
    }
    if key == "allow-hosts" {
        if values.is_empty() {
            return Err(
                LineError::new(after_span(first_span), "expected one or more host globs")
                    .expecting(["a host glob like *.example.com"]),
            );
        }
        return Ok(FileOption::AllowHosts(values));
    }
    if values.len() > 1 {
        return Err(
            LineError::new(values[1].span, "expected end of line").expecting(["a single value"])
        );
    }
    let Some(value) = values.pop() else {
        return Err(
            LineError::new(after_span(first_span), "expected a value").expecting(["a value"])
        );
    };
    let duration = |value| {
        option_shape(value, |text| text.parse::<DurationLit>().ok(), &[
            "a duration like 500ms or 10s",
        ])
    };
    match key.as_str() {
        "base" => Ok(FileOption::Base(value)),
        "browser" => {
            let parse = |text: &str| text.parse::<BrowserKind>().ok();
            Ok(FileOption::Browser(option_shape(value, parse, &[
                "chromium", "firefox", "webkit",
            ])?))
        }
        "viewport" => Ok(FileOption::Viewport(option_shape(
            value,
            |text| text.parse::<Viewport>().ok(),
            &["WIDTHxHEIGHT like 1280x800"],
        )?)),
        "step-timeout" => Ok(FileOption::StepTimeout(duration(value)?)),
        "entry-timeout" => Ok(FileOption::EntryTimeout(duration(value)?)),
        "nav-timeout" => Ok(FileOption::NavTimeout(duration(value)?)),
        "dialogs" => {
            let parse = |text: &str| text.parse::<DialogPolicy>().ok();
            Ok(FileOption::Dialogs(option_shape(value, parse, &[
                "dismiss", "accept",
            ])?))
        }
        "reduced-motion" => {
            let parse = |text: &str| text.parse::<ReducedMotion>().ok();
            Ok(FileOption::ReducedMotion(option_shape(value, parse, &[
                "reduce",
                "no-preference",
            ])?))
        }
        "storage" => Ok(FileOption::Storage(value)),
        "user-agent" => Ok(FileOption::UserAgent(value)),
        "setup" => Ok(FileOption::Setup(value)),
        "model" => Ok(FileOption::Model(value)),
        key => unreachable!("option key `{key}` was validated against OPTION_KEYS"),
    }
}

/// Where the parser is in the file structure (SPEC 4).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum State {
    /// Before `[Options]` and the first entry.
    Preamble,
    /// Inside the `[Options]` section.
    Options,
    /// Inside an entry's actions.
    Actions,
    /// After an entry's `PAGE` line.
    AfterPage,
    /// After an entry's first check line.
    Checks,
}

/// The keywords that start a check line (SPEC 9, 10).
const CHECK_KEYWORDS: [&str; 3] = ["ASSERT", "JUDGE", "CAPTURE"];

struct Parser {
    options:        Vec<OptionLine>,
    entries:        Vec<Entry>,
    comments:       Vec<Comment>,
    current:        Option<Entry>,
    state:          State,
    options_header: Option<u32>,
    seen_visit:     bool,
}

/// True when a line starts with the `[Options]` header.
fn is_options_header(trimmed: &str) -> bool {
    trimmed.split_whitespace().next() == Some("[Options]")
}

fn structural_line(line: &str) -> bool {
    let first = line.split_whitespace().next().unwrap_or_default();
    first.starts_with('[')
        || first == "PAGE"
        || ACTION_KEYWORDS.contains(&first)
        || CHECK_KEYWORDS.contains(&first)
}

/// A body that spans lines, split line by line with `split`:
/// [`json_segments`] for a JSON body and [`bare_segments`] for a fenced one.
/// A JSON string cannot hold a line break, so no string crosses a line.
fn multiline_value(
    lines: &[&str],
    start: usize,
    text: &str,
    split: fn(&str, u32) -> Result<Vec<ValueSegment>, LineError>,
) -> Result<Value, LineError> {
    let mut segments = Vec::new();
    for (offset, line) in text.split('\n').enumerate() {
        if offset > 0 {
            segments.push(ValueSegment::Literal("\n".to_owned()));
        }
        let line_no = u32::try_from(start + offset + 1).unwrap_or(u32::MAX);
        match split(line, 1) {
            Ok(line_segments) => segments.extend(line_segments),
            Err(error) => {
                return Err(error.at_source(
                    line_no,
                    lines.get(start + offset).copied().unwrap_or_default(),
                ));
            }
        }
    }
    Ok(Value {
        segments: merge_literals(segments),
        span:     Span {
            line:   u32::try_from(start + 1).unwrap_or(u32::MAX),
            column: 1,
            len:    u32::try_from(lines.get(start).map_or(0, |line| line.chars().count()))
                .unwrap_or(u32::MAX)
                .max(1),
        },
        quoted:   false,
    })
}

fn json_for_validation(text: &str) -> Result<String, LineError> {
    let chars: Vec<char> = text.chars().collect();
    let mut out = String::with_capacity(text.len());
    let mut in_string = false;
    let mut escaped = false;
    let mut pos = 0;
    while pos < chars.len() {
        let ch = chars[pos];
        // `\\{{` inside a string is an escaped backslash before a
        // reference, not the `\{{` escape.
        if ch == '\\'
            && !escaped
            && chars.get(pos + 1) == Some(&'{')
            && chars.get(pos + 2) == Some(&'{')
        {
            out.push('{');
            out.push('{');
            pos += 3;
            continue;
        }
        if ch == '{' && chars.get(pos + 1) == Some(&'{') {
            let close = chars[pos + 2..]
                .windows(2)
                .position(|pair| pair == ['}', '}'])
                .ok_or_else(|| {
                    LineError::new(
                        Span {
                            line:   0,
                            column: 1,
                            len:    2,
                        },
                        "unterminated `{{` variable reference",
                    )
                })?;
            if in_string {
                out.push_str("whirl");
            } else {
                out.push_str("null");
            }
            pos += close + 4;
            continue;
        }
        out.push(ch);
        if in_string {
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == '"' {
                in_string = false;
            }
        } else if ch == '"' {
            in_string = true;
        }
        pos += 1;
    }
    Ok(out)
}

fn json_body_end(lines: &[&str], start: usize) -> Result<usize, LineError> {
    let mut stack = Vec::new();
    let mut in_string = false;
    let mut escaped = false;
    let mut interpolation = false;
    let mut previous = '\0';
    for (index, line) in lines.iter().enumerate().skip(start) {
        for ch in line.chars() {
            if interpolation {
                if previous == '}' && ch == '}' {
                    interpolation = false;
                }
                previous = ch;
                continue;
            }
            if !in_string && previous == '{' && ch == '{' {
                let _ = stack.pop();
                interpolation = true;
                previous = ch;
                continue;
            }
            if in_string {
                if escaped {
                    escaped = false;
                } else if ch == '\\' {
                    escaped = true;
                } else if ch == '"' {
                    in_string = false;
                }
            } else {
                match ch {
                    '"' => in_string = true,
                    '{' => stack.push('}'),
                    '[' => stack.push(']'),
                    '}' | ']' => {
                        if stack.pop() != Some(ch) {
                            return Err(LineError::new(
                                Span {
                                    line:   0,
                                    column: 1,
                                    len:    1,
                                },
                                "mismatched delimiter in HTTP JSON body",
                            )
                            .at_source(u32::try_from(index + 1).unwrap_or(u32::MAX), *line));
                        }
                        if stack.is_empty() {
                            return Ok(index);
                        }
                    }
                    _ => {}
                }
            }
            previous = ch;
        }
        previous = '\n';
    }
    Err(LineError::new(
        Span {
            line:   0,
            column: 1,
            len:    1,
        },
        "unterminated HTTP JSON body",
    )
    .at_source(
        u32::try_from(start + 1).unwrap_or(u32::MAX),
        lines.get(start).copied().unwrap_or_default(),
    ))
}

/// Parses a locator written on its own, such as one the AI cache holds
/// (SPEC 12.1). Every segment needs a prefix.
pub(crate) fn parse_locator(text: &str) -> Result<Locator, String> {
    let mut cursor = Cursor::new(text, 1);
    let mut tokens = Vec::new();
    while let Some(token) = cursor.next_token().map_err(|error| error.message)? {
        tokens.push(token);
    }
    let end = Span {
        line:   1,
        column: 1,
        len:    1,
    };
    build_locator(tokens, false, end).map_err(|error| error.message)
}

/// Parses one action line written on its own, such as one the AI cache
/// holds (SPEC 12.1).
pub(crate) fn parse_action_line(text: &str) -> Result<Action, String> {
    let mut cursor = Cursor::new(text, 1);
    cursor.skip_ws();
    let start = cursor.pos;
    let first = cursor
        .next_token()
        .map_err(|error| error.message)?
        .ok_or_else(|| "an empty line".to_owned())?;
    let keyword = first
        .bare_single()
        .filter(|keyword| ACTION_KEYWORDS.contains(keyword))
        .ok_or_else(|| format!("`{text}` is not an action line"))?
        .to_owned();
    let (kind, timeout) =
        parse_action_body(&keyword, first.span, &mut cursor).map_err(|error| error.message)?;
    let (text, span) = cursor.content(start);
    Ok(Action {
        kind,
        timeout,
        line: 1,
        span,
        text,
    })
}

/// Parses one `.whirl` source, stopping at the file's first error.
pub(crate) fn parse_file(path: &Path, source: &str) -> Result<File, ParseError> {
    let mut parser = Parser {
        options:        Vec::new(),
        entries:        Vec::new(),
        comments:       Vec::new(),
        current:        None,
        state:          State::Preamble,
        options_header: None,
        seen_visit:     false,
    };
    let lines: Vec<&str> = source.lines().collect();
    let mut index = 0;
    while index < lines.len() {
        let line = lines[index];
        let line_no = u32::try_from(index + 1).unwrap_or(u32::MAX);
        parser
            .parse_line(line, line_no)
            .map_err(|error| into_parse_error(path, error, line_no, line))?;
        if matches!(line.split_whitespace().next(), Some("HTTP" | "MOCK")) {
            index = parser.parse_http_tail(path, &lines, index)?;
        } else if line.split_whitespace().next() == Some("EXTRACT") {
            index = parser.parse_extract_schema(path, &lines, index)?;
        } else {
            index += 1;
        }
    }
    if parser.awaiting_visit() {
        let mock = parser
            .current
            .as_ref()
            .and_then(|entry| entry.actions.last())
            .expect("an entry of MOCK lines has an action");
        return Err(ParseError {
            code:        ParseErrorCode::Syntax,
            path:        path.to_path_buf(),
            line:        mock.line,
            column:      mock.span.column,
            len:         mock.span.len,
            source_line: lines
                .get(usize::try_from(mock.line.saturating_sub(1)).unwrap_or(0))
                .copied()
                .unwrap_or_default()
                .to_owned(),
            message:     "MOCK lines must be followed by VISIT".to_owned(),
            expected:    vec!["MOCK".to_owned(), "VISIT".to_owned()],
        });
    }
    if let Some(entry) = parser.current.take() {
        parser.entries.push(entry);
    }
    if parser.entries.is_empty() {
        let line_no = u32::try_from(lines.len().max(1)).unwrap_or(u32::MAX);
        return Err(ParseError {
            code:        ParseErrorCode::Syntax,
            path:        path.to_path_buf(),
            line:        line_no,
            column:      1,
            len:         1,
            source_line: lines.last().copied().unwrap_or_default().to_owned(),
            message:     "a file needs at least one entry".to_owned(),
            expected:    vec!["HTTP".to_owned(), "VISIT".to_owned()],
        });
    }
    let mut file = File {
        path:           path.to_path_buf(),
        options:        parser.options,
        entries:        parser.entries,
        comments:       parser.comments,
        options_header: parser.options_header,
    };
    mark_text_extracts(&mut file);
    Ok(file)
}

/// Marks each `extract:NAME` subject whose latest earlier `EXTRACT` line
/// of that name has no schema, so the value is a string (SPEC 7.6, 9.6).
fn mark_text_extracts(file: &mut File) {
    let mut text: HashMap<String, bool> = HashMap::new();
    for entry in &mut file.entries {
        for action in &entry.actions {
            if let ActionKind::Extract { name, schema, .. } = &action.kind {
                text.insert(name.text.clone(), schema.is_none());
            }
        }
        for check in &mut entry.checks {
            let subject = match check {
                CheckStep::Assert(Assert {
                    body: AssertBody::Check(line),
                    ..
                }) => &mut line.subject,
                CheckStep::Capture(capture) => &mut capture.subject,
                CheckStep::Assert(_) | CheckStep::Judge(_) => continue,
            };
            if let Subject::Extract { name, text: plain } = subject {
                *plain = text.get(&name.text).copied().unwrap_or(false);
            }
        }
    }
}

fn into_parse_error(path: &Path, error: LineError, line_no: u32, line: &str) -> ParseError {
    ParseError {
        code:        ParseErrorCode::Syntax,
        path:        path.to_path_buf(),
        line:        error.line.unwrap_or(line_no),
        column:      error.column,
        len:         error.len,
        source_line: error.source.unwrap_or_else(|| line.to_owned()),
        message:     error.message,
        expected:    error.expected,
    }
}

impl Parser {
    /// Parses the optional JSON Schema object below an `EXTRACT` headline
    /// (SPEC 7.6) and returns the index of the next line to parse.
    fn parse_extract_schema(
        &mut self,
        path: &Path,
        lines: &[&str],
        headline: usize,
    ) -> Result<usize, ParseError> {
        let mut index = headline + 1;
        let mut comments = Vec::new();
        while let Some(line) = lines.get(index) {
            let trimmed = line.trim_start();
            if trimmed.is_empty() || trimmed.starts_with('#') {
                comments.push(index);
                index += 1;
                continue;
            }
            break;
        }
        let Some(line) = lines.get(index).copied() else {
            return Ok(headline + 1);
        };
        let trimmed = line.trim_start();
        if !trimmed.starts_with('{') {
            if trimmed.starts_with('[') && !is_options_header(trimmed) {
                let error = LineError::new(
                    Span {
                        line:   0,
                        column: 1,
                        len:    1,
                    },
                    "an EXTRACT schema is a JSON object",
                )
                .at_source(u32::try_from(index + 1).unwrap_or(u32::MAX), line);
                return Err(into_parse_error(path, error, 0, line));
            }
            return Ok(headline + 1);
        }
        for comment in comments {
            let line_no = u32::try_from(comment + 1).unwrap_or(u32::MAX);
            self.parse_line(lines[comment], line_no)
                .map_err(|error| into_parse_error(path, error, line_no, lines[comment]))?;
        }
        let end =
            json_body_end(lines, index).map_err(|error| into_parse_error(path, error, 0, line))?;
        let text = lines[index..=end].join("\n");
        if let Some(offset) = lines[index..=end]
            .iter()
            .position(|line| line.contains("{{"))
        {
            let source = lines[index + offset];
            let column = source
                .find("{{")
                .map_or(1, |byte| source[..byte].chars().count() + 1);
            let error = LineError::new(
                Span {
                    line:   0,
                    column: u32::try_from(column).unwrap_or(u32::MAX),
                    len:    2,
                },
                "an EXTRACT schema cannot contain `{{ }}`",
            )
            .at_source(
                u32::try_from(index + offset + 1).unwrap_or(u32::MAX),
                source,
            );
            return Err(into_parse_error(path, error, 0, source));
        }
        if let Err(error) = serde_json::from_str::<serde_json::Value>(&text) {
            let error_line = index + error.line();
            let source = lines
                .get(error_line.saturating_sub(1))
                .copied()
                .unwrap_or(line);
            let local = LineError::new(
                Span {
                    line:   0,
                    column: u32::try_from(error.column()).unwrap_or(u32::MAX),
                    len:    1,
                },
                format!("invalid EXTRACT schema: {error}"),
            )
            .at_source(u32::try_from(error_line).unwrap_or(u32::MAX), source);
            return Err(into_parse_error(path, local, 0, source));
        }
        let action = self
            .current
            .as_mut()
            .and_then(|entry| entry.actions.last_mut())
            .expect("an EXTRACT headline creates a current action");
        let ActionKind::Extract { schema, .. } = &mut action.kind else {
            unreachable!("parse_extract_schema follows an EXTRACT action");
        };
        *schema = Some(ExtractSchema {
            text,
            line: u32::try_from(index + 1).unwrap_or(u32::MAX),
            end_line: u32::try_from(end + 1).unwrap_or(u32::MAX),
        });
        Ok(end + 1)
    }

    fn parse_http_tail(
        &mut self,
        path: &Path,
        lines: &[&str],
        headline: usize,
    ) -> Result<usize, ParseError> {
        let mut headers: Vec<HttpHeader> = Vec::new();
        let mut body = None;
        let mut index = headline + 1;
        let mut last_component = headline;

        while index < lines.len() {
            let line = lines[index];
            let trimmed = line.trim_start();
            if trimmed.is_empty() {
                index += 1;
                continue;
            }
            if trimmed.starts_with('#') {
                let line_no = u32::try_from(index + 1).unwrap_or(u32::MAX);
                self.parse_line(line, line_no)
                    .map_err(|error| into_parse_error(path, error, line_no, line))?;
                index += 1;
                continue;
            }
            // A JSON array body starts with `[`, as `[Options]` does.
            let json_start = trimmed.starts_with('{')
                || (trimmed.starts_with('[') && !is_options_header(trimmed));
            if !json_start && structural_line(line) {
                break;
            }

            if body.is_some() {
                let duplicate_body = trimmed == "```" || json_start;
                let header_after_body = trimmed
                    .split_whitespace()
                    .next()
                    .and_then(|token| token.strip_suffix(':'))
                    .is_some_and(is_attr_name);
                if duplicate_body || header_after_body {
                    let message = if duplicate_body {
                        "duplicate HTTP body"
                    } else {
                        "an HTTP header cannot follow its body"
                    };
                    let error = LineError::new(
                        Span {
                            line:   0,
                            column: 1,
                            len:    u32::try_from(trimmed.chars().next().map_or(1, |_| {
                                trimmed
                                    .split_whitespace()
                                    .next()
                                    .unwrap_or(trimmed)
                                    .chars()
                                    .count()
                            }))
                            .unwrap_or(u32::MAX),
                        },
                        message,
                    )
                    .at_source(u32::try_from(index + 1).unwrap_or(u32::MAX), line);
                    return Err(into_parse_error(path, error, 0, line));
                }
                break;
            }

            if trimmed == "```" {
                let Some(close) = lines[index + 1..]
                    .iter()
                    .position(|line| line.trim() == "```")
                    .map(|offset| index + offset + 1)
                else {
                    let error = LineError::new(
                        Span {
                            line:   0,
                            column: 1,
                            len:    3,
                        },
                        "unterminated fenced HTTP body",
                    )
                    .expecting(["a closing ``` line"])
                    .at_source(u32::try_from(index + 1).unwrap_or(u32::MAX), line);
                    return Err(into_parse_error(path, error, 0, line));
                };
                let text = lines[index + 1..close].join("\n");
                let value_start = (index + 1).min(lines.len().saturating_sub(1));
                let value = multiline_value(lines, value_start, &text, bare_segments)
                    .map_err(|error| into_parse_error(path, error, 0, line))?;
                body = Some(HttpBody {
                    kind: HttpBodyKind::Text,
                    value,
                    text,
                    line: u32::try_from(index + 1).unwrap_or(u32::MAX),
                    end_line: u32::try_from(close + 1).unwrap_or(u32::MAX),
                });
                last_component = close;
                index = close + 1;
                continue;
            }

            if json_start {
                let end = json_body_end(lines, index)
                    .map_err(|error| into_parse_error(path, error, 0, line))?;
                let text = lines[index..=end].join("\n");
                let validation = json_for_validation(&text)
                    .map_err(|error| into_parse_error(path, error, 0, line))?;
                if let Err(error) = serde_json::from_str::<serde_json::Value>(&validation) {
                    let error_line = index + error.line();
                    let source = lines
                        .get(error_line.saturating_sub(1))
                        .copied()
                        .unwrap_or(line);
                    let local = LineError::new(
                        Span {
                            line:   0,
                            column: u32::try_from(error.column()).unwrap_or(u32::MAX),
                            len:    1,
                        },
                        format!("invalid HTTP JSON body: {error}"),
                    )
                    .at_source(u32::try_from(error_line).unwrap_or(u32::MAX), source);
                    return Err(into_parse_error(path, local, 0, source));
                }
                let value = multiline_value(lines, index, &text, json_segments)
                    .map_err(|error| into_parse_error(path, error, 0, line))?;
                body = Some(HttpBody {
                    kind: HttpBodyKind::Json,
                    value,
                    text,
                    line: u32::try_from(index + 1).unwrap_or(u32::MAX),
                    end_line: u32::try_from(end + 1).unwrap_or(u32::MAX),
                });
                last_component = end;
                index = end + 1;
                continue;
            }

            let line_no = u32::try_from(index + 1).unwrap_or(u32::MAX);
            let mut cursor = Cursor::new(line, line_no);
            cursor.skip_ws();
            let first = cursor
                .next_token()
                .map_err(|error| into_parse_error(path, error, line_no, line))?
                .expect("a non-blank HTTP header line has a token");
            let Some(name) = first.bare_single().and_then(|text| text.strip_suffix(':')) else {
                break;
            };
            if !is_attr_name(name)
                || headers
                    .iter()
                    .any(|header| header.name.eq_ignore_ascii_case(name))
            {
                let error = LineError::new(first.span, "invalid or duplicate HTTP header name");
                return Err(into_parse_error(path, error, line_no, line));
            }
            let value = cursor
                .next_token()
                .map_err(|error| into_parse_error(path, error, line_no, line))?
                .ok_or_else(|| {
                    into_parse_error(
                        path,
                        LineError::new(first.span, "expected an HTTP header value"),
                        line_no,
                        line,
                    )
                })?
                .into_value()
                .map_err(|error| into_parse_error(path, error, line_no, line))?;
            if let Some(extra) = cursor
                .next_token()
                .map_err(|error| into_parse_error(path, error, line_no, line))?
            {
                let error = LineError::new(extra.span, "expected end of header line")
                    .expecting(["a single value"]);
                return Err(into_parse_error(path, error, line_no, line));
            }
            if let Some(comment) = cursor.take_comment() {
                self.comments.push(comment);
            }
            headers.push(HttpHeader {
                name: name.to_owned(),
                value,
                line: line_no,
            });
            last_component = index;
            index += 1;
        }

        let source = lines[headline..=last_component].join("\n");
        let action = self
            .current
            .as_mut()
            .and_then(|entry| entry.actions.last_mut())
            .expect("an HTTP or MOCK headline creates a current action");
        match &mut action.kind {
            ActionKind::Http {
                headers: action_headers,
                body: action_body,
                source: action_source,
                ..
            }
            | ActionKind::Mock {
                response:
                    MockResponse::Fulfill {
                        headers: action_headers,
                        body: action_body,
                        ..
                    },
                source: action_source,
                ..
            } => {
                *action_headers = headers;
                *action_body = body;
                *action_source = source;
            }
            ActionKind::Mock {
                response: MockResponse::Failed,
                source: action_source,
                ..
            } => {
                if last_component != headline {
                    let line = lines[headline + 1..=last_component]
                        .iter()
                        .position(|line| {
                            let trimmed = line.trim_start();
                            !trimmed.is_empty() && !trimmed.starts_with('#')
                        })
                        .map_or(headline + 1, |offset| headline + 1 + offset);
                    let error = LineError::new(
                        Span {
                            line:   0,
                            column: 1,
                            len:    1,
                        },
                        "a failed MOCK takes no header lines and no body",
                    )
                    .at_source(u32::try_from(line + 1).unwrap_or(u32::MAX), lines[line]);
                    return Err(into_parse_error(path, error, 0, lines[line]));
                }
                *action_source = source;
            }
            _ => unreachable!("parse_http_tail follows an HTTP or MOCK action"),
        }
        Ok(index)
    }

    fn parse_line(&mut self, line: &str, line_no: u32) -> Result<(), LineError> {
        let mut cursor = Cursor::new(line, line_no);
        cursor.skip_ws();
        let content_start = cursor.pos;
        match cursor.peek() {
            None => return Ok(()),
            Some('#') => {
                let column = cursor.column();
                let text: String = cursor.chars[cursor.pos + 1..].iter().collect();
                self.comments.push(Comment {
                    line: line_no,
                    column,
                    text,
                    own_line: true,
                });
                return Ok(());
            }
            Some(_) => {}
        }
        let first = cursor
            .next_token()?
            .expect("a non-blank, non-comment line has a first token");
        let header = first
            .bare_single()
            .filter(|text| text.starts_with('['))
            .map(str::to_owned);
        if let Some(text) = header {
            self.handle_header(&text, first.span)?;
        } else {
            self.handle_step(&first, &mut cursor, content_start)?;
        }
        if let Some(comment) = cursor.take_comment() {
            self.comments.push(comment);
        }
        if let Some(extra) = cursor.next_token()? {
            return Err(
                LineError::new(extra.span, "expected end of line").expecting(["end of line"])
            );
        }
        Ok(())
    }

    fn handle_header(&mut self, text: &str, span: Span) -> Result<(), LineError> {
        match text {
            "[Options]" => {
                if self.state == State::Preamble && self.options_header.is_none() {
                    self.options_header = Some(span.line);
                    self.state = State::Options;
                    Ok(())
                } else {
                    Err(
                        LineError::new(span, "`[Options]` may appear once, before the first entry")
                            .expecting(["an action"]),
                    )
                }
            }
            other => {
                Err(LineError::new(span, format!("unknown section `{other}`"))
                    .expecting(["[Options]"]))
            }
        }
    }

    /// True when the current entry is an independent HTTP request, whose
    /// checks read its response without a name (SPEC 7.3).
    fn in_http_entry(&self) -> bool {
        self.current.as_ref().is_some_and(|entry| {
            matches!(
                entry.actions.first().map(|action| &action.kind),
                Some(ActionKind::Http { .. })
            )
        })
    }

    /// True when the current entry holds only `MOCK` lines, before the
    /// file's first `VISIT`: it needs a `VISIT` next (SPEC 4).
    fn awaiting_visit(&self) -> bool {
        !self.seen_visit && self.current.is_some() && !self.in_http_entry()
    }

    /// Parses an `ASSERT` or `CAPTURE` line after its keyword (SPEC 9,
    /// 10).
    fn handle_check_line(
        &mut self,
        keyword: &str,
        keyword_span: Span,
        cursor: &mut Cursor,
        content_start: usize,
    ) -> Result<(), LineError> {
        match self.state {
            State::Preamble | State::Options => {
                return Err(LineError::new(
                    keyword_span,
                    format!("`{keyword}` must follow an entry's actions"),
                )
                .expecting(["an action"]));
            }
            State::Actions | State::AfterPage | State::Checks => {}
        }
        if self.awaiting_visit() {
            return Err(LineError::new(
                keyword_span,
                format!("`{keyword}` needs an earlier VISIT"),
            )
            .expecting(["MOCK", "VISIT"]));
        }
        let implicit_http = self.in_http_entry();
        let line_no = cursor.line_no;
        let check = if keyword == "JUDGE" {
            if implicit_http {
                return Err(LineError::new(
                    keyword_span,
                    "JUDGE checks the page; an HTTP entry has no page",
                ));
            }
            let mut tokens = Vec::new();
            while let Some(token) = cursor.next_token()? {
                tokens.push(token);
            }
            let timeout = split_timeout(&mut tokens);
            let (scope, instruction) = parse_act(Args::new(tokens, keyword_span))?;
            let (text, span) = cursor.content(content_start);
            CheckStep::Judge(Judge {
                scope,
                claim: instruction,
                timeout,
                line: line_no,
                span,
                text,
            })
        } else if keyword == "ASSERT" {
            let Some(first) = cursor.next_token()? else {
                return Err(LineError::new(after_span(keyword_span), "expected a check")
                    .expecting([
                        "a locator",
                        "url",
                        "title",
                        "eval",
                        "response:NAME",
                        "window:NAME",
                    ]));
            };
            let (body, timeout) = parse_assert_body(first, cursor, implicit_http)?;
            let (text, span) = cursor.content(content_start);
            CheckStep::Assert(Assert {
                body,
                timeout,
                line: line_no,
                span,
                text,
            })
        } else {
            let name = capture_name(cursor.next_token()?, keyword_span)?;
            let mut capture = parse_capture_body(name, cursor, implicit_http)?;
            let (text, span) = cursor.content(content_start);
            capture.line = line_no;
            capture.span = span;
            capture.text = text;
            CheckStep::Capture(capture)
        };
        let entry = self
            .current
            .as_mut()
            .expect("the entry states always have a current entry");
        entry.checks.push(check);
        self.state = State::Checks;
        Ok(())
    }

    fn handle_step(
        &mut self,
        first: &RawToken,
        cursor: &mut Cursor,
        content_start: usize,
    ) -> Result<(), LineError> {
        let line_no = cursor.line_no;
        let bare = first.bare_single().map(str::to_owned);
        if let Some(keyword) = bare.as_deref().filter(|text| CHECK_KEYWORDS.contains(text)) {
            return self.handle_check_line(keyword, first.span, cursor, content_start);
        }
        if let Some(keyword) = bare
            .as_deref()
            .filter(|text| ACTION_KEYWORDS.contains(text))
        {
            let keyword = keyword.to_owned();
            let first_span = first.span;
            let (kind, timeout) = parse_action_body(&keyword, first_span, cursor)?;
            let is_http = matches!(kind, ActionKind::Http { .. });
            let is_visit = matches!(kind, ActionKind::Visit { .. });
            let is_mock = matches!(kind, ActionKind::Mock { .. });
            if !self.seen_visit && !is_http && !is_visit && !is_mock {
                return Err(
                    LineError::new(first_span, "a browser action needs an earlier VISIT")
                        .expecting(["HTTP", "VISIT"]),
                );
            }
            if is_visit {
                self.seen_visit = true;
            }
            let (text, span) = cursor.content(content_start);
            let action = Action {
                kind,
                timeout,
                line: line_no,
                span,
                text,
            };
            let current_is_http = self.current.as_ref().is_some_and(|entry| {
                matches!(
                    entry.actions.first().map(|action| &action.kind),
                    Some(ActionKind::Http { .. })
                )
            });
            if self.state == State::Actions && !is_http && !current_is_http {
                let entry = self
                    .current
                    .as_mut()
                    .expect("the Actions state always has a current entry");
                entry.actions.push(action);
            } else {
                if self.awaiting_visit() {
                    return Err(
                        LineError::new(first_span, "MOCK lines must be followed by VISIT")
                            .expecting(["MOCK", "VISIT"]),
                    );
                }
                if let Some(entry) = self.current.take() {
                    self.entries.push(entry);
                }
                self.current = Some(Entry {
                    actions: vec![action],
                    page:    None,
                    checks:  Vec::new(),
                });
                self.state = State::Actions;
            }
            return Ok(());
        }
        let is_page = bare.as_deref() == Some("PAGE");
        match self.state {
            State::Preamble => Err(LineError::new(
                first.span,
                "expected `[Options]`, HTTP, or VISIT",
            )
            .expecting(["[Options]", "HTTP", "VISIT"])),
            State::Options => {
                let first_span = first.span;
                let option = parse_option_line(first, cursor)?;
                let (_, span) = cursor.content(content_start);
                if let FileOption::Snapshot(snapshot) = &option {
                    validate_snapshot_option(
                        self.options.iter().filter_map(|line| {
                            if let FileOption::Snapshot(option) = &line.option {
                                Some(option)
                            } else {
                                None
                            }
                        }),
                        snapshot,
                        first_span,
                    )?;
                }
                self.options.push(OptionLine {
                    option,
                    line: line_no,
                    span,
                });
                Ok(())
            }
            State::Actions if is_page => {
                if self.in_http_entry() {
                    return Err(LineError::new(first.span, "an HTTP entry cannot have PAGE")
                        .expecting(["ASSERT", "CAPTURE", "an action"]));
                }
                if self.awaiting_visit() {
                    return Err(LineError::new(first.span, "`PAGE` needs an earlier VISIT")
                        .expecting(["MOCK", "VISIT"]));
                }
                let (check, timeout) = parse_page_body(first.span, cursor)?;
                let (text, span) = cursor.content(content_start);
                let entry = self
                    .current
                    .as_mut()
                    .expect("the Actions state always has a current entry");
                entry.page = Some(Page {
                    check,
                    timeout,
                    line: line_no,
                    span,
                    text,
                });
                self.state = State::AfterPage;
                Ok(())
            }
            State::Actions
                if self.current.as_ref().is_some_and(|entry| {
                    matches!(
                        entry.actions.last().map(|action| &action.kind),
                        Some(ActionKind::Snapshot { .. })
                    )
                }) =>
            {
                let first_span = first.span;
                let option = parse_snapshot_option(first, cursor)?;
                let (text, span) = cursor.content(content_start);
                let action = self
                    .current
                    .as_mut()
                    .and_then(|entry| entry.actions.last_mut())
                    .expect("snapshot action");
                let ActionKind::Snapshot { options, .. } = &mut action.kind else {
                    unreachable!("snapshot action")
                };
                validate_snapshot_option(
                    options.iter().map(|line| &line.option),
                    &option,
                    first_span,
                )?;
                options.push(SnapshotOptionLine {
                    option,
                    line: line_no,
                    span,
                });
                action.text.push('\n');
                action.text.push_str(&text);
                Ok(())
            }
            State::Actions => Err(
                LineError::new(first.span, "expected a step line").expecting([
                    "an action",
                    "PAGE",
                    "ASSERT",
                    "CAPTURE",
                ]),
            ),
            State::AfterPage if is_page => Err(LineError::new(
                first.span,
                "an entry has one PAGE line",
            )
            .expecting(["an action", "ASSERT", "CAPTURE"])),
            State::Checks if is_page => Err(LineError::new(
                first.span,
                "`PAGE` must come before an entry's check lines",
            )
            .expecting(["an action", "ASSERT", "CAPTURE"])),
            State::AfterPage | State::Checks => Err(LineError::new(
                first.span,
                "expected a step line",
            )
            .expecting(["an action", "ASSERT", "CAPTURE"])),
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::check::PredicateKind;
    use crate::lang::ast::DurationUnit;
    /// Parses many `.whirl` sources and reports every broken file's first
    /// error, so one `whirl check` run surfaces them all (SPEC 13, 16).
    fn parse_files<'a>(
        files: impl IntoIterator<Item = (&'a Path, &'a str)>,
    ) -> Result<Vec<File>, Vec<ParseError>> {
        let mut parsed = Vec::new();
        let mut errors = Vec::new();
        for (path, source) in files {
            match parse_file(path, source) {
                Ok(file) => parsed.push(file),
                Err(error) => errors.push(error),
            }
        }
        if errors.is_empty() {
            Ok(parsed)
        } else {
            Err(errors)
        }
    }

    use super::*;
    use crate::lang::ast::DefaultEngine;

    fn parse(source: &str) -> File {
        parse_file(Path::new("test.whirl"), source).expect("source should parse")
    }

    fn parse_err(source: &str) -> ParseError {
        parse_file(Path::new("test.whirl"), source).expect_err("source should fail to parse")
    }

    fn only_entry(file: &File) -> &Entry {
        assert_eq!(file.entries.len(), 1, "expected one entry");
        &file.entries[0]
    }

    fn action_kind(source_tail: &str) -> ActionKind {
        let source = format!("VISIT /\n{source_tail}\n");
        let file = parse(&source);
        let entry = only_entry(&file);
        entry.actions[1].kind.clone()
    }

    fn lit(value: &Value) -> String {
        value.as_literal().expect("value should be literal")
    }

    fn default_segment_text(locator: &Locator) -> String {
        assert_eq!(locator.segments.len(), 1);
        let SegmentKind::Default(value) = &locator.segments[0].kind else {
            panic!(
                "expected a default-engine segment, got {:?}",
                locator.segments[0].kind
            );
        };
        lit(value)
    }

    fn only_assert(source_tail: &str) -> AssertBody {
        let source = format!("VISIT /\nASSERT {source_tail}\n");
        let file = parse(&source);
        only_entry(&file).asserts().collect::<Vec<_>>()[0]
            .body
            .clone()
    }

    #[test]
    fn every_state_check_parses() {
        let states = [
            ("visible", StateCheck::Visible),
            ("hidden", StateCheck::Hidden),
            ("enabled", StateCheck::Enabled),
            ("disabled", StateCheck::Disabled),
            ("checked", StateCheck::Checked),
            ("unchecked", StateCheck::Unchecked),
            ("focused", StateCheck::Focused),
        ];
        for (keyword, expected) in states {
            let body = only_assert(&format!("testid:widget {keyword}"));
            let AssertBody::ElementState { locator: _, state } = body else {
                panic!("expected a state check for {keyword}");
            };
            assert_eq!(state, expected);
        }
    }

    /// The check line of a single-assert file.
    fn only_check(line: &str) -> CheckLine {
        let AssertBody::Check(check) = only_assert(line) else {
            panic!("expected a check line: {line}");
        };
        check
    }

    fn expected_literal(check: &CheckLine) -> String {
        let PredicateSpec::Compare {
            expected: Operand::Value(value),
            ..
        } = &check.predicate
        else {
            panic!("expected a compared value");
        };
        lit(value)
    }

    #[test]
    fn element_value_checks_parse() {
        let check = only_check("testid:x text == Alice");
        assert!(matches!(check.subject, Subject::Element {
            extractor: Extractor::Text,
            ..
        }));
        assert_eq!(check.predicate.kind(), PredicateKind::Eq);
        assert_eq!(expected_literal(&check), "Alice");

        let check = only_check("label:Amount value != \"0\"");
        assert!(matches!(check.subject, Subject::Element {
            extractor: Extractor::Value,
            ..
        }));
        assert_eq!(check.predicate.kind(), PredicateKind::Ne);

        let check = only_check("testid:x attr:aria-expanded contains tru");
        assert!(matches!(&check.subject, Subject::Element {
            extractor: Extractor::Attr(name),
            ..
        } if name == "aria-expanded"));
        assert_eq!(check.predicate.kind(), PredicateKind::Contains);

        let check = only_check("testid:order text matches /Order #\\w+/i");
        let PredicateSpec::Matches(regex) = check.predicate else {
            panic!("expected matches");
        };
        assert_eq!(regex.pattern, "Order #\\w+");
        assert!(regex.flags.ignore_case);
        assert!(!regex.flags.dot_all);
    }

    #[test]
    fn every_comparison_parses() {
        for (op_text, expected) in COMPARE_KEYWORDS {
            let check = only_check(&format!("testid:row count {op_text} 3"));
            assert!(matches!(check.subject, Subject::Element {
                extractor: Extractor::Count,
                ..
            }));
            assert_eq!(check.predicate.kind(), *expected, "{op_text}");
            assert_eq!(expected_literal(&check), "3");
        }
    }

    #[test]
    fn every_word_predicate_parses_with_and_without_not() {
        for (name, kind) in WORD_PREDICATES {
            let check = only_check(&format!("response:r json:$.a {name}"));
            assert_eq!(check.predicate.kind(), *kind);
            assert!(!check.negated);
            let check = only_check(&format!("response:r json:$.a not {name}"));
            assert!(check.negated, "not {name}");
        }
    }

    #[test]
    fn url_and_title_checks_parse() {
        let check = only_check("url contains \"q=widget\"");
        assert_eq!(check.subject, Subject::Url);
        assert_eq!(expected_literal(&check), "q=widget");
        let check = only_check("title == \"Checkout\"");
        assert_eq!(check.subject, Subject::Title);
        assert_eq!(expected_literal(&check), "Checkout");
    }

    #[test]
    fn filters_parse_with_their_arguments() {
        let check = only_check(
            "testid:total text replace , \"\" replaceRegex /[^0-9]/ x split / nth -1 toInt >= 10",
        );
        let kinds: Vec<FilterKind> = check.filters.iter().map(|filter| filter.kind).collect();
        assert_eq!(kinds, vec![
            FilterKind::Replace,
            FilterKind::ReplaceRegex,
            FilterKind::Split,
            FilterKind::Nth,
            FilterKind::ToInt
        ]);
        assert_eq!(check.filters[3].args, vec![FilterArg::Index(-1)]);
        assert_eq!(check.predicate.kind(), PredicateKind::Ge);
        let check = only_check("url urlQueryParam page == 2");
        assert_eq!(check.filters[0].kind, FilterKind::UrlQueryParam);
    }

    #[test]
    fn eval_subjects_and_json_paths_parse() {
        let check = only_check("eval \"window.dataLayer\" json:$[?@.event=='buy'] count == 1");
        let Subject::Eval(script) = &check.subject else {
            panic!("expected an eval subject");
        };
        assert_eq!(lit(script), "window.dataLayer");
        assert_eq!(check.filters[0].kind, FilterKind::Json);
        assert_eq!(check.filters[1].kind, FilterKind::Count);
    }

    #[test]
    fn json_literals_parse_on_one_line() {
        let check = only_check("response:r json:$.size == {\"h\": 20, \"w\": [1, 2]} @3s");
        let PredicateSpec::Compare {
            expected: Operand::Json(literal),
            ..
        } = &check.predicate
        else {
            panic!("expected a JSON literal");
        };
        assert_eq!(literal.text, r#"{"h": 20, "w": [1, 2]}"#);
        let check = only_check("response:r json:$.tags == [\"{{tag}}\", {{n}}]");
        let PredicateSpec::Compare {
            expected: Operand::Json(literal),
            ..
        } = &check.predicate
        else {
            panic!("expected a JSON literal");
        };
        assert!(
            literal
                .value
                .segments
                .iter()
                .any(|segment| matches!(segment, ValueSegment::Var(name) if name == "n"))
        );
        let error = parse_err("VISIT /\nASSERT response:r json:$.a == [1, 2\n");
        assert_eq!(error.message, "unterminated JSON literal");
        let error = parse_err("VISIT /\nASSERT response:r json:$.a == {\"a\" 1}\n");
        assert!(
            error.message.starts_with("invalid JSON literal"),
            "{}",
            error.message
        );
    }

    #[test]
    fn a_variable_reference_is_not_a_json_literal() {
        let check = only_check("response:r json:$.id == {{order_id}}");
        let PredicateSpec::Compare {
            expected: Operand::Value(value),
            ..
        } = &check.predicate
        else {
            panic!("expected a value");
        };
        assert_eq!(value.segments, vec![ValueSegment::Var(
            "order_id".to_owned()
        )]);
    }

    #[test]
    fn json_paths_are_checked_when_literal() {
        let error = parse_err("VISIT /\nASSERT response:x json:$.[ == 1\n");
        assert!(
            error.message.starts_with("invalid JSONPath"),
            "{}",
            error.message
        );
        let error = parse_err("VISIT /\nASSERT response:r json:$[\"a\"] == 1\n");
        assert!(
            error.message.contains("quote the whole value"),
            "{}",
            error.message
        );
        only_check("response:r json:\"$[?@.name == 'Ada Lovelace']\" count == 1");
        // A quote joins the argument after a colon, as after any prefix.
        let check = only_check("response:r json:$[?@.time=='12:\"00 am']\" exists");
        let Subject::Response {
            field: ResponseField::Json(path),
            ..
        } = check.subject
        else {
            panic!("expected a JSONPath field");
        };
        assert_eq!(path.as_literal().as_deref(), Some("$[?@.time=='12:00 am']"));
    }

    #[test]
    fn malformed_bytes_literals_fail_only_in_typed_checks() {
        let error = parse_err("HTTP GET /x\nASSERT status == 200\nASSERT bytes == hex,zz;\n");
        assert_eq!(error.message, "invalid bytes literal \"hex,zz;\"");
        let error = parse_err("HTTP GET /x\nASSERT status == 200\nASSERT json:$.t == base64,@@;\n");
        assert_eq!(error.message, "invalid bytes literal \"base64,@@;\"");
        // Text checks and quoted values take the text as written.
        only_check("response:r body == hex,zz;");
        only_check("response:r json:$.t == \"hex,zz;\"");
        only_check("response:r bytes startsWith hex,89504e47;");
    }

    #[test]
    fn xpath_fields_and_filters_parse() {
        let check = only_check("response:r xpath://_:entry count == 2");
        assert!(matches!(check.subject, Subject::Response {
            field: ResponseField::Xpath(_),
            ..
        }));
        let check = only_check("testid:list attr:data-xml xpath:\"string(//li[@class='x'])\" == A");
        assert_eq!(check.filters[0].kind, FilterKind::Xpath);
        let check = only_check("response:r xpath:{{expr}} exists");
        assert!(matches!(check.subject, Subject::Response {
            field: ResponseField::Xpath(_),
            ..
        }));
    }

    #[test]
    fn xpath_expressions_are_checked_when_literal() {
        for invalid in ["//li[", "count(", "foo()"] {
            let error = parse_err(&format!(
                "VISIT /\nASSERT response:x xpath:\"{invalid}\" exists\n"
            ));
            assert!(
                error.message.starts_with("invalid XPath expression"),
                "{invalid}: {}",
                error.message
            );
        }
        let error = parse_err("VISIT /\nASSERT response:r xpath://a[@x=\"1\"] exists\n");
        assert!(
            error.message.contains("quote the whole value"),
            "{}",
            error.message
        );
    }

    #[test]
    fn regexes_must_be_valid_in_unicode_mode() {
        let error = parse_err("VISIT /\nASSERT url matches /a\\-b/\n");
        assert!(
            error.message.starts_with("invalid regex in Unicode mode"),
            "{}",
            error.message
        );
    }

    #[test]
    fn a_missing_predicate_is_an_error() {
        let error = parse_err("VISIT /\nASSERT url toString\n");
        assert_eq!(error.message, "expected a predicate");
        let error = parse_err("VISIT /\nASSERT url bogus\n");
        assert_eq!(error.message, "expected a filter or a predicate");
    }

    #[test]
    fn assert_lines_take_timeout_suffixes() {
        let source = "VISIT /\nASSERT testid:x visible @2500ms\n";
        let file = parse(source);
        let assert_line = &only_entry(&file).asserts().collect::<Vec<_>>()[0];
        assert_eq!(
            assert_line.timeout,
            Some(DurationLit {
                amount: 2500,
                unit:   DurationUnit::Milliseconds,
            })
        );
    }

    #[test]
    fn a_bare_value_cannot_start_with_an_at_sign() {
        // SPEC 3.1: a bare token that starts with `@` is always the step
        // timeout, so it must be a duration at the end of the line.
        for (line, message) in [
            (
                "FILL Email @zzz",
                "a bare value cannot start with `@`; quote it",
            ),
            ("FILL Email @5s x", "a step timeout must end the line"),
            (
                "FILL Email x @99999999999999999999s",
                "the step timeout is too long",
            ),
        ] {
            let error = parse_err(&format!("VISIT /\n{line}\n"));
            assert_eq!(error.message, message, "{line}");
        }
        let ActionKind::Fill { value, .. } = action_kind("FILL Email \"@zzz\"") else {
            panic!("expected FILL");
        };
        assert_eq!(lit(&value), "@zzz");
    }

    #[test]
    fn regex_flags_allow_only_i_s_m() {
        let PredicateSpec::Matches(regex) = only_check("url matches /a.b/ism").predicate else {
            panic!("expected matches");
        };
        assert!(regex.flags.ignore_case && regex.flags.dot_all && regex.flags.multiline);

        let error = parse_err("VISIT /\nASSERT url matches /a/g\n");
        assert_eq!(error.message, "invalid regex flag `g`");
        assert_eq!(error.expected, vec![
            "i".to_owned(),
            "s".to_owned(),
            "m".to_owned()
        ]);
    }

    #[test]
    fn a_hash_inside_a_regex_is_literal() {
        let PredicateSpec::Matches(regex) = only_check("url matches /a#b/").predicate else {
            panic!("expected matches");
        };
        assert_eq!(regex.pattern, "a#b");
    }

    #[test]
    fn capture_sources_parse() {
        let source = "VISIT /\nCAPTURE heading: heading:\"Hi\" text\nCAPTURE input: label:Email value\nCAPTURE rows: testid:row count\nCAPTURE href: link:* attr:href\nCAPTURE here: url\nCAPTURE name: title\nCAPTURE result: eval \"1 + 1\"\n";
        let file = parse(source);
        let captures = &only_entry(&file).captures().collect::<Vec<_>>();
        assert_eq!(captures.len(), 7);
        assert!(matches!(&captures[0].subject, Subject::Element {
            extractor: Extractor::Text,
            ..
        }));
        assert!(matches!(&captures[1].subject, Subject::Element {
            extractor: Extractor::Value,
            ..
        }));
        assert!(matches!(&captures[2].subject, Subject::Element {
            extractor: Extractor::Count,
            ..
        }));
        let Subject::Element {
            extractor: Extractor::Attr(attr),
            ..
        } = &captures[3].subject
        else {
            panic!("expected an attr extractor");
        };
        assert_eq!(attr, "href");
        assert!(matches!(&captures[4].subject, Subject::Url));
        assert!(matches!(&captures[5].subject, Subject::Title));
        let Subject::Eval(script) = &captures[6].subject else {
            panic!("expected an eval source");
        };
        assert_eq!(lit(script), "1 + 1");
        assert_eq!(captures[6].name.text, "result");
    }

    #[test]
    fn capture_filters_and_timeout_parse() {
        let source =
            "VISIT /\nCAPTURE order_id: testid:confirmation text regex /Order #(\\w+)/ toInt @5s\n";
        let file = parse(source);
        let capture = &only_entry(&file).captures().collect::<Vec<_>>()[0];
        let FilterArg::Regex(regex) = &capture.filters[0].args[0] else {
            panic!("expected a regex argument");
        };
        assert_eq!(regex.pattern, "Order #(\\w+)");
        assert_eq!(capture.filters[1].kind, FilterKind::ToInt);
        assert_eq!(
            capture.timeout,
            Some(DurationLit {
                amount: 5,
                unit:   DurationUnit::Seconds,
            })
        );
    }

    #[test]
    fn artifact_names_may_contain_hyphens() {
        let ActionKind::Screenshot { name } = action_kind("SCREENSHOT after-verification-code")
        else {
            panic!("expected SCREENSHOT");
        };
        assert_eq!(name.text, "after-verification-code");
        let ActionKind::Snapshot { name, .. } = action_kind("SNAPSHOT top-bar_v2") else {
            panic!("expected SNAPSHOT");
        };
        assert_eq!(name.text, "top-bar_v2");
        for bad in [
            "SCREENSHOT -leading",
            "SCREENSHOT with.dot",
            "SCREENSHOT \"quoted\"",
        ] {
            let error = parse_err(&format!("VISIT /\n{bad}\n"));
            assert_eq!(error.message, "expected a name", "source: {bad}");
        }
    }

    #[test]
    fn capture_names_must_be_identifiers() {
        let error = parse_err("VISIT /\nCAPTURE 9lives: url\n");
        assert!(
            error.message.contains("capture"),
            "message: {}",
            error.message
        );
    }

    const SPEC_EXAMPLE: &str = r#"# checkout.whirl — buy a widget as a signed-in user.
[Options]
base: https://shop.example.com
viewport: 1280x800

# Log in.
VISIT /login

FILL "Email" alice@example.com
FILL "Password" {{env.TEST_PASSWORD}}
CLICK button:"Sign in"
PAGE /dashboard
ASSERT heading:"Welcome back" visible
ASSERT testid:user-menu text == Alice

# Find a product.
FILL placeholder:"Search products" widget
PRESS Enter
ASSERT url contains "q=widget"
ASSERT testid:result-card count >= 1
CAPTURE first_product: testid:result-card >> nth:1 >> link:* attr:href

# Add it to the cart.
VISIT {{first_product}}
CLICK "Add to cart"
ASSERT testid:cart-badge text == 1
ASSERT alert:* text contains "Added to cart"
"#;

    #[test]
    fn the_spec_example_parses_end_to_end() {
        let file = parse(SPEC_EXAMPLE);
        assert_eq!(file.options.len(), 2);
        assert!(matches!(file.options[0].option, FileOption::Base(_)));
        assert!(matches!(
            file.options[1].option,
            FileOption::Viewport(OptionValue::Literal(Viewport {
                width:  1280,
                height: 800,
            }))
        ));

        assert_eq!(file.entries.len(), 3);
        let [login, search, cart] = file.entries.as_slice() else {
            panic!("expected three entries");
        };

        assert_eq!(login.actions.len(), 4);
        assert!(matches!(login.actions[0].kind, ActionKind::Visit { .. }));
        assert!(matches!(login.actions[1].kind, ActionKind::Fill { .. }));
        assert!(matches!(login.actions[2].kind, ActionKind::Fill { .. }));
        assert!(matches!(login.actions[3].kind, ActionKind::Click { .. }));
        let page = login
            .page
            .as_ref()
            .expect("the first entry has a PAGE line");
        let PageCheck::Value(value) = &page.check else {
            panic!("expected a PAGE value");
        };
        assert_eq!(lit(value), "/dashboard");
        assert_eq!(login.asserts().collect::<Vec<_>>().len(), 2);
        assert!(matches!(
            login.asserts().collect::<Vec<_>>()[0].body,
            AssertBody::ElementState {
                state: StateCheck::Visible,
                ..
            }
        ));
        assert!(login.captures().collect::<Vec<_>>().is_empty());

        assert_eq!(search.actions.len(), 2);
        assert!(matches!(search.actions[1].kind, ActionKind::Press {
            target: None,
            ..
        }));
        assert_eq!(search.asserts().collect::<Vec<_>>().len(), 2);
        assert!(matches!(
            &search.asserts().collect::<Vec<_>>()[0].body,
            AssertBody::Check(CheckLine {
                subject: Subject::Url,
                predicate: PredicateSpec::Compare {
                    kind: PredicateKind::Contains,
                    ..
                },
                ..
            })
        ));
        assert!(matches!(
            &search.asserts().collect::<Vec<_>>()[1].body,
            AssertBody::Check(CheckLine {
                subject: Subject::Element {
                    extractor: Extractor::Count,
                    ..
                },
                predicate: PredicateSpec::Compare {
                    kind: PredicateKind::Ge,
                    ..
                },
                ..
            })
        ));
        assert_eq!(search.captures().collect::<Vec<_>>().len(), 1);
        let capture = &search.captures().collect::<Vec<_>>()[0];
        assert_eq!(capture.name.text, "first_product");
        let Subject::Element {
            locator,
            extractor: Extractor::Attr(attr),
        } = &capture.subject
        else {
            panic!("expected an attr capture");
        };
        assert_eq!(locator.segments.len(), 3);
        assert_eq!(attr, "href");

        assert_eq!(cart.actions.len(), 2);
        let ActionKind::Visit { url } = &cart.actions[0].kind else {
            panic!("expected VISIT");
        };
        assert_eq!(url.segments, vec![ValueSegment::Var(
            "first_product".to_owned()
        )]);
        assert_eq!(cart.asserts().collect::<Vec<_>>().len(), 2);
    }

    #[test]
    fn entries_are_named_by_the_nearest_comment_above() {
        let file = parse(SPEC_EXAMPLE);
        let names: Vec<String> = file
            .entries
            .iter()
            .map(|entry| file.entry_display_name(entry))
            .collect();
        assert_eq!(names, vec![
            "Log in.".to_owned(),
            "Find a product.".to_owned(),
            "Add it to the cart.".to_owned(),
        ]);
    }

    #[test]
    fn entry_names_fall_back_to_the_first_action_and_line() {
        let source = "VISIT /a\nASSERT url == \"/a\"\nCLICK \"Next\"\n";
        let file = parse(source);
        assert_eq!(
            file.entry_display_name(&file.entries[0]),
            "VISIT /a (line 1)"
        );
        assert_eq!(
            file.entry_display_name(&file.entries[1]),
            "CLICK \"Next\" (line 3)"
        );
    }

    #[test]
    fn a_comment_above_an_earlier_step_does_not_name_a_later_entry() {
        let source = "# Log in.\nVISIT /a\nASSERT url == \"/a\"\nCLICK \"Next\"\n";
        let file = parse(source);
        assert_eq!(file.entry_display_name(&file.entries[0]), "Log in.");
        assert_eq!(
            file.entry_display_name(&file.entries[1]),
            "CLICK \"Next\" (line 4)"
        );
    }

    #[test]
    fn parse_errors_render_with_a_caret_and_alternatives() {
        let error = parse_err("[Options]\nspeed: fast\nVISIT /\n");
        let rendered = error.render();
        let lines: Vec<&str> = rendered.lines().collect();
        assert_eq!(
            lines[0],
            "test.whirl:2:1: error: unknown option key `speed`"
        );
        assert_eq!(lines[1], "  speed: fast");
        assert_eq!(lines[2], "  ^^^^^");
        assert!(lines[3].starts_with("  expected one of: base, browser,"));
    }

    #[test]
    fn the_caret_sits_under_the_offending_token() {
        let error = parse_err("VISIT /\nCLICK testid:card >> nth:x\n");
        let rendered = error.render();
        let lines: Vec<&str> = rendered.lines().collect();
        assert_eq!(lines[1], "  CLICK testid:card >> nth:x");
        assert_eq!(lines[2], "                       ^^^^^");
    }

    #[test]
    fn parse_files_reports_every_broken_file() {
        let good = "VISIT /\n";
        let bad_one = "CLICK \"Go\"\n";
        let bad_two = "VISIT /\nCLICK nth:1\n";
        let inputs = [
            (Path::new("good.whirl"), good),
            (Path::new("bad-one.whirl"), bad_one),
            (Path::new("bad-two.whirl"), bad_two),
        ];
        let errors = parse_files(inputs).expect_err("two files should fail");
        assert_eq!(errors.len(), 2);
        assert_eq!(errors[0].path, Path::new("bad-one.whirl"));
        assert_eq!(errors[1].path, Path::new("bad-two.whirl"));

        let inputs = [(Path::new("good.whirl"), good)];
        let files = parse_files(inputs).expect("one good file should parse");
        assert_eq!(files.len(), 1);
    }

    #[test]
    fn trailing_comments_are_recorded_but_not_part_of_the_step() {
        let source = "VISIT /login # go home\n";
        let file = parse(source);
        let action = &only_entry(&file).actions[0];
        assert_eq!(action.text, "VISIT /login");
        assert_eq!(file.comments.len(), 1);
        assert!(!file.comments[0].own_line);
        assert_eq!(file.comments[0].text, " go home");
    }

    #[test]
    fn a_hash_inside_a_quoted_string_is_literal() {
        let ActionKind::Fill { target: _, value } = action_kind("FILL \"Note\" \"a # b\"") else {
            panic!("expected FILL");
        };
        assert_eq!(lit(&value), "a # b");
    }

    #[test]
    fn a_hash_inside_a_token_is_text() {
        let ActionKind::Visit { url } = action_kind("VISIT /docs#install") else {
            panic!("expected VISIT");
        };
        assert_eq!(lit(&url), "/docs#install");
        let ActionKind::Click { target, .. } = action_kind("CLICK css:#submit") else {
            panic!("expected CLICK");
        };
        let SegmentKind::Css(selector) = &target.segments[0].kind else {
            panic!("expected a css segment");
        };
        assert_eq!(lit(selector), "#submit");
        // A comment starts only at a `#` after white space (SPEC 3).
        let file = parse("VISIT /\nCLICK Save # a note\n");
        assert_eq!(file.comments.len(), 1);
        for (line, message) in [
            (
                "FILL Note \"a\"#b",
                "expected white space after the closing quote",
            ),
            ("ASSERT url matches /a/i#b", "invalid regex flag `#`"),
            ("CLICK #submit", "expected a locator"),
        ] {
            let error = parse_err(&format!("VISIT /\n{line}\n"));
            assert_eq!(error.message, message, "{line}");
        }
    }

    #[test]
    fn crlf_line_endings_parse() {
        let file = parse("VISIT /login\r\nCLICK \"Go\"\r\n");
        assert_eq!(only_entry(&file).actions.len(), 2);
        assert_eq!(only_entry(&file).actions[0].text, "VISIT /login");
    }

    #[test]
    fn a_timeout_suffix_follows_the_value_it_does_not_replace() {
        let source = "VISIT /\nFILL \"Email\" alice@example.com @5s\n";
        let file = parse(source);
        let action = &only_entry(&file).actions[1];
        let ActionKind::Fill { target: _, value } = &action.kind else {
            panic!("expected FILL");
        };
        assert_eq!(lit(value), "alice@example.com");
        assert_eq!(
            action.timeout,
            Some(DurationLit {
                amount: 5,
                unit:   DurationUnit::Seconds,
            })
        );
    }

    #[test]
    fn a_browser_action_needs_an_earlier_visit() {
        let error = parse_err("CLICK \"Go\"\n");
        assert_eq!(error.message, "a browser action needs an earlier VISIT");
        assert_eq!(error.expected, vec!["HTTP".to_owned(), "VISIT".to_owned()]);
        assert_eq!((error.line, error.column), (1, 1));
    }

    #[test]
    fn http_entries_can_run_before_visit_or_without_a_page() {
        let file = parse(
            "HTTP POST /fixtures\n{\n  \"name\": \"Ada\"\n}\nASSERT status == 201\nCAPTURE id: json:$.id\nVISIT /users/{{id}}\n",
        );
        assert_eq!(file.entries.len(), 2);
        assert!(matches!(
            file.entries[0].actions[0].kind,
            ActionKind::Http { .. }
        ));

        let http_only = parse("HTTP GET /health\nASSERT status == 200\n");
        assert_eq!(http_only.entries.len(), 1);
    }

    #[test]
    fn http_blocks_parse_headers_json_and_fenced_text() {
        let file = parse(
            r#"HTTP POST /fixtures
Authorization: "Bearer {{env.TOKEN}}"
{
  "name": "{{name}}",
  "enabled": true
}
ASSERT status == 201
ASSERT json:$.name == {{name}}
CAPTURE id: json:$.id
HTTP POST /imports
Content-Type: text/plain
```
first # literal
second\line
```
ASSERT status == 202
"#,
        );
        assert_eq!(file.entries.len(), 2);
        let ActionKind::Http { headers, body, .. } = &file.entries[0].actions[0].kind else {
            panic!("expected HTTP");
        };
        assert_eq!(headers.len(), 1);
        assert_eq!(headers[0].name, "Authorization");
        let body = body.as_ref().expect("JSON body");
        assert_eq!(body.kind, HttpBodyKind::Json);
        assert!(body.text.contains("\"enabled\": true"));
        assert!(matches!(
            &file.entries[0].asserts().collect::<Vec<_>>()[0].body,
            AssertBody::Check(CheckLine {
                subject: Subject::Response {
                    name:  None,
                    field: ResponseField::Status,
                },
                ..
            })
        ));
        assert!(matches!(
            &file.entries[0].captures().collect::<Vec<_>>()[0].subject,
            Subject::Response {
                name:  None,
                field: ResponseField::Json(_),
            }
        ));

        let ActionKind::Http { body, .. } = &file.entries[1].actions[0].kind else {
            panic!("expected HTTP");
        };
        let body = body.as_ref().expect("text body");
        assert_eq!(body.kind, HttpBodyKind::Text);
        assert_eq!(body.text, "first # literal\nsecond\\line");
    }

    #[test]
    fn malformed_http_blocks_report_the_body_line() {
        let error = parse_err("HTTP POST /\n{\n  \"missing\":,\n}\n");
        assert_eq!(error.line, 3);
        assert!(error.message.contains("invalid HTTP JSON body"));

        let error = parse_err("HTTP POST /\n```\nnever closed\n");
        assert_eq!(error.line, 2);
        assert_eq!(error.message, "unterminated fenced HTTP body");

        let error = parse_err("HTTP POST /\n{}\nX-Test: late\n");
        assert_eq!(error.line, 3);
        assert_eq!(error.message, "an HTTP header cannot follow its body");
    }

    #[test]
    fn later_entries_may_start_with_any_action() {
        let source = "VISIT /\nASSERT url == \"/\"\nCLICK \"Next\"\n";
        let file = parse(source);
        assert_eq!(file.entries.len(), 2);
    }

    #[test]
    fn an_action_after_asserts_starts_a_new_entry() {
        let source =
            "VISIT /a\nCLICK \"One\"\nASSERT testid:x visible\nVISIT /b\nASSERT testid:y visible\n";
        let file = parse(source);
        assert_eq!(file.entries.len(), 2);
        assert_eq!(file.entries[0].actions.len(), 2);
        assert_eq!(file.entries[0].asserts().collect::<Vec<_>>().len(), 1);
        assert_eq!(file.entries[1].actions.len(), 1);
        assert_eq!(file.entries[1].asserts().collect::<Vec<_>>().len(), 1);
    }

    #[test]
    fn an_action_after_page_starts_a_new_entry() {
        let source = "VISIT /a\nPAGE /a\nCLICK \"Next\"\n";
        let file = parse(source);
        assert_eq!(file.entries.len(), 2);
        assert!(file.entries[0].page.is_some());
        assert!(file.entries[1].page.is_none());
    }

    #[test]
    fn an_action_after_captures_starts_a_new_entry() {
        let source = "VISIT /a\nCAPTURE here: url\nCLICK \"Next\"\n";
        let file = parse(source);
        assert_eq!(file.entries.len(), 2);
    }

    #[test]
    fn comments_and_blank_lines_do_not_split_an_entry() {
        let source = "VISIT /a\n\n# a comment\n\nCLICK \"Next\"\n";
        let file = parse(source);
        let entry = only_entry(&file);
        assert_eq!(entry.actions.len(), 2);
        assert_eq!(file.comments.len(), 1);
        assert!(file.comments[0].own_line);
    }

    #[test]
    fn check_lines_keep_their_written_order() {
        let file = parse(
            "VISIT /\nPAGE /\nCAPTURE id: testid:x text\nASSERT testid:y text == {{id}} @5s\nASSERT window:main closed\n",
        );
        let entry = only_entry(&file);
        let lines: Vec<(u32, bool)> = entry
            .checks
            .iter()
            .map(|check| (check.line(), matches!(check, CheckStep::Assert(_))))
            .collect();
        assert_eq!(lines, [(3, false), (4, true), (5, true)]);
        let asserts = entry.asserts().collect::<Vec<_>>();
        assert_eq!(asserts[0].text, "ASSERT testid:y text == {{id}} @5s");
        assert_eq!(asserts[0].timeout.map(DurationLit::millis), Some(5000));
    }

    #[test]
    fn http_entries_take_check_lines() {
        let file =
            parse("HTTP GET /api\nAccept: text/plain\nASSERT status == 200\nCAPTURE n: json:$.n\n");
        let entry = only_entry(&file);
        let ActionKind::Http { headers, .. } = &entry.actions[0].kind else {
            panic!("expected an HTTP action");
        };
        assert_eq!(headers.len(), 1);
        assert_eq!(entry.checks.len(), 2);
    }

    #[test]
    fn an_action_after_a_check_line_starts_a_new_entry() {
        let file = parse("VISIT /\nASSERT url == /\nCLICK Next\nASSERT url == /next\n");
        assert_eq!(file.entries.len(), 2);
        assert_eq!(file.entries[1].checks.len(), 1);
    }

    #[test]
    fn check_lines_need_an_entry() {
        let error = parse_err("[Options]\nbase: http://x\nASSERT url == /\n");
        assert_eq!(error.line, 3);
        assert!(error.message.contains("must follow an entry's actions"));
    }

    #[test]
    fn page_cannot_follow_a_check_line() {
        let error = parse_err("VISIT /\nASSERT url == /\nPAGE /\n");
        assert_eq!(error.line, 3);
        assert_eq!(
            error.message,
            "`PAGE` must come before an entry's check lines"
        );
    }

    #[test]
    fn check_keywords_need_a_body() {
        assert_eq!(parse_err("VISIT /\nASSERT\n").message, "expected a check");
        assert_eq!(
            parse_err("VISIT /\nCAPTURE url\n").message,
            "expected a capture name"
        );
    }

    #[test]
    fn the_removed_sections_are_unknown() {
        for header in ["[Asserts]", "[Captures]"] {
            let error = parse_err(&format!("VISIT /\n{header}\nurl == /\n"));
            assert_eq!(error.message, format!("unknown section `{header}`"));
        }
    }

    #[test]
    fn mock_parses_a_status_headers_and_a_body() {
        let file = parse(
            "MOCK GET /api/flags 200\nX-Test: yes\n{ \"on\": true }\nVISIT /\nASSERT url == /\n",
        );
        let entry = only_entry(&file);
        assert_eq!(entry.actions.len(), 2);
        let ActionKind::Mock {
            method,
            url,
            response:
                MockResponse::Fulfill {
                    status,
                    headers,
                    body,
                },
            source,
        } = &entry.actions[0].kind
        else {
            panic!("expected a fulfilling mock");
        };
        assert_eq!(method, "GET");
        assert_eq!(lit(url), "/api/flags");
        assert_eq!(*status, 200);
        assert_eq!(headers[0].name, "X-Test");
        assert_eq!(
            body.as_ref().map(|body| body.kind),
            Some(HttpBodyKind::Json)
        );
        assert_eq!(
            source,
            "MOCK GET /api/flags 200\nX-Test: yes\n{ \"on\": true }"
        );
    }

    #[test]
    fn mock_parses_a_failed_request() {
        let file = parse("VISIT /\nMOCK POST https://x.test/* failed\nCLICK Go\n");
        let ActionKind::Mock { response, .. } = &file.entries[0].actions[1].kind else {
            panic!("expected a mock");
        };
        assert_eq!(*response, MockResponse::Failed);
    }

    #[test]
    fn malformed_mocks_are_parse_errors() {
        for (source, message) in [
            ("MOCK GET /a 200 @5s\n", "MOCK has no step timeout"),
            (
                "MOCK GET /a\n",
                "expected a status code from 200 to 599, or `failed`",
            ),
            (
                "MOCK get /a 200\n",
                "expected an uppercase HTTP method like GET or POST",
            ),
            (
                "MOCK GET /a 99\n",
                "expected a status code from 200 to 599, or `failed`",
            ),
            (
                "MOCK GET /a 600\n",
                "expected a status code from 200 to 599, or `failed`",
            ),
            (
                "MOCK GET /a failed\nX-Test: yes\n",
                "a failed MOCK takes no header lines and no body",
            ),
            (
                "MOCK GET /a failed\n{}\n",
                "a failed MOCK takes no header lines and no body",
            ),
        ] {
            assert_eq!(parse_err(source).message, message, "source: {source}");
        }
        let error = parse_err("MOCK GET /a failed\n# note\nX-Test: yes\n");
        assert_eq!(error.line, 3);
    }

    #[test]
    fn http_and_mock_bodies_can_be_json_arrays() {
        let file = parse("HTTP POST /a\n[1, {{n}}]\nMOCK GET /b 200\n[\n  \"x\"\n]\nVISIT /\n");
        let ActionKind::Http { body, .. } = &file.entries[0].actions[0].kind else {
            panic!("expected HTTP");
        };
        let body = body.as_ref().expect("JSON body");
        assert_eq!(body.kind, HttpBodyKind::Json);
        assert_eq!(body.text, "[1, {{n}}]");
        let ActionKind::Mock {
            response: MockResponse::Fulfill { body, .. },
            ..
        } = &file.entries[1].actions[0].kind
        else {
            panic!("expected a fulfilling mock");
        };
        assert_eq!(
            body.as_ref().map(|body| body.text.as_str()),
            Some("[\n  \"x\"\n]")
        );
    }

    #[test]
    fn a_backslash_before_a_reference_in_a_json_string_is_an_escaped_backslash() {
        let file = parse("HTTP POST /a\n{\"a\": \"\\\\{{x}}\", \"b\": \"\\{{y}}\"}\n");
        let ActionKind::Http { body, .. } = &file.entries[0].actions[0].kind else {
            panic!("expected HTTP");
        };
        assert_eq!(body.as_ref().expect("JSON body").value.segments, vec![
            ValueSegment::Literal("{\"a\": \"\\\\".to_owned()),
            ValueSegment::Var("x".to_owned()),
            ValueSegment::Literal("\", \"b\": \"{{y}}\"}".to_owned()),
        ]);

        let AssertBody::Check(CheckLine {
            predicate:
                PredicateSpec::Compare {
                    expected: Operand::Json(literal),
                    ..
                },
            ..
        }) = only_assert("url == [\"\\\\{{x}}\"]")
        else {
            panic!("expected a JSON literal");
        };
        assert_eq!(literal.value.segments, vec![
            ValueSegment::Literal("[\"\\\\".to_owned()),
            ValueSegment::Var("x".to_owned()),
            ValueSegment::Literal("\"]".to_owned()),
        ]);
    }

    #[test]
    fn mock_lines_need_a_visit_after_them() {
        for (source, message) in [
            ("MOCK GET /a 204\n", "MOCK lines must be followed by VISIT"),
            (
                "MOCK GET /a 204\nHTTP GET /b\nVISIT /\n",
                "MOCK lines must be followed by VISIT",
            ),
            (
                "MOCK GET /a 204\nASSERT url == x\nVISIT /\n",
                "`ASSERT` needs an earlier VISIT",
            ),
            ("MOCK GET /a 204\nPAGE /\n", "`PAGE` needs an earlier VISIT"),
        ] {
            assert_eq!(parse_err(source).message, message, "{source}");
        }
        let error = parse_err("MOCK GET /a 204\n# the end\n");
        assert_eq!((error.line, error.column), (1, 1));
        assert_eq!(
            parse("HTTP GET /b\nMOCK GET /a 204\nVISIT /\n")
                .entries
                .len(),
            2
        );
        assert_eq!(parse("VISIT /\nPAGE /\nMOCK GET /a 204\n").entries.len(), 2);
    }

    #[test]
    fn mock_lines_come_before_the_first_visit_only() {
        assert_eq!(parse("MOCK GET /a 204\nVISIT /\n").entries.len(), 1);
        let error = parse_err("MOCK GET /a 204\nCLICK Go\n");
        assert_eq!(error.message, "a browser action needs an earlier VISIT");
    }

    #[test]
    fn request_subjects_parse_every_field() {
        let file = parse(
            "VISIT /\nRESPONSE r GET /a\nASSERT request:r method == GET\nASSERT request:r url contains a\nASSERT request:r header:x-id == 1\nASSERT request:r body isEmpty\nASSERT request:r bytes count == 0\nASSERT request:r json:$.qty == 1\nCAPTURE q: request:r xpath:\"string(//q)\"\n",
        );
        let fields: Vec<RequestField> = only_entry(&file)
            .checks
            .iter()
            .map(|check| {
                let subject = match check {
                    CheckStep::Assert(Assert {
                        body: AssertBody::Check(line),
                        ..
                    }) => &line.subject,
                    CheckStep::Capture(capture) => &capture.subject,
                    CheckStep::Assert(_) | CheckStep::Judge(_) => panic!("expected a subject"),
                };
                let Subject::Request { name, field } = subject else {
                    panic!("expected a request subject");
                };
                assert_eq!(name.text, "r");
                field.clone()
            })
            .collect();
        assert!(matches!(fields[..], [
            RequestField::Method,
            RequestField::Url,
            RequestField::Header(_),
            RequestField::Body,
            RequestField::Bytes,
            RequestField::Json(_),
            RequestField::Xpath(_),
        ]));
        assert_eq!(
            parse_err("VISIT /\nASSERT request:r status == 200\n").message,
            "expected a request field"
        );
        assert_eq!(
            parse_err("HTTP GET /a\nASSERT request:r method == GET\n").message,
            "an HTTP entry can only check its response"
        );
    }

    #[test]
    fn ai_segments_parse_last_in_a_locator() {
        let file = parse(
            "VISIT /\nCLICK dialog:* >> ai:\"the {{which}} email field\"\nASSERT ai:\"the total\" text == 1\n",
        );
        let locators = file.locator_uses();
        let description = locators[0]
            .locator
            .ai_description()
            .expect("the click has an ai: target");
        assert_eq!(description.segments.len(), 3);
        assert_eq!(locators[0].locator.segments.len(), 2);
        assert!(locators[1].locator.ai_description().is_some());
        assert!(file.uses_ai());
    }

    #[test]
    fn misplaced_ai_segments_are_parse_errors() {
        assert_eq!(
            parse_err("VISIT /\nCLICK ai:\"a row\" >> button:*\n").message,
            "`ai:` must be the last segment of a locator"
        );
        assert_eq!(
            parse_err("VISIT /\nSNAPSHOT page\nsnapshot-mask: ai:\"the clock\"\n").message,
            "`ai:` cannot be a snapshot mask"
        );
        assert_eq!(
            parse_err("VISIT /\nCLICK ai:\n").message,
            "`ai:` needs a value"
        );
    }

    #[test]
    fn extract_parses_a_name_a_scope_an_instruction_and_a_schema() {
        let file = parse(
            "VISIT /\nEXTRACT order testid:summary \"the total\" @30s\n# the shape\n{\n  \"type\": \"number\"\n}\nASSERT extract:order > 0\n",
        );
        let entry = only_entry(&file);
        let ActionKind::Extract {
            name,
            scope,
            instruction,
            schema,
        } = &entry.actions[1].kind
        else {
            panic!("expected EXTRACT");
        };
        assert_eq!(name.text, "order");
        assert!(scope.is_some());
        assert_eq!(lit(instruction), "the total");
        let schema = schema.as_ref().expect("a schema");
        assert_eq!((schema.line, schema.end_line), (4, 6));
        assert_eq!(schema.json(), serde_json::json!({"type": "number"}));
        assert_eq!(
            entry.actions[1].timeout.map(DurationLit::millis),
            Some(30_000)
        );
        assert!(matches!(
            &entry.checks[0],
            CheckStep::Assert(Assert {
                body: AssertBody::Check(CheckLine {
                    subject: Subject::Extract { text: false, .. },
                    ..
                }),
                ..
            })
        ));
    }

    #[test]
    fn an_extract_without_a_schema_reads_text() {
        let file = parse("VISIT /\nEXTRACT note \"the note\"\nASSERT extract:note == 42\n");
        assert!(matches!(
            &only_entry(&file).checks[0],
            CheckStep::Assert(Assert {
                body: AssertBody::Check(CheckLine {
                    subject: Subject::Extract { text: true, .. },
                    ..
                }),
                ..
            })
        ));
    }

    #[test]
    fn judge_is_a_check_line_with_an_optional_scope() {
        let file = parse(
            "VISIT /\nASSERT testid:summary visible\nJUDGE testid:summary \"the total is {{total}}\" @20s\nJUDGE \"no error shows\"\nCAPTURE t: url\n",
        );
        let entry = only_entry(&file);
        let judges: Vec<&Judge> = entry.judges().collect();
        assert_eq!(judges.len(), 2);
        assert!(judges[0].scope.is_some());
        assert_eq!(judges[0].line, 3);
        assert_eq!(judges[0].timeout.map(DurationLit::millis), Some(20_000));
        assert_eq!(
            judges[0].text,
            "JUDGE testid:summary \"the total is {{total}}\" @20s"
        );
        assert!(judges[1].scope.is_none());
        assert_eq!(lit(&judges[1].claim), "no error shows");
        assert!(matches!(entry.checks[3], CheckStep::Capture(_)));
        assert!(file.uses_judge() && file.uses_ai());
    }

    #[test]
    fn goal_takes_one_value_and_a_timeout() {
        let file = parse("VISIT /\nGOAL \"buy {{item}}\" @180s\nASSERT url exists\n");
        let action = &only_entry(&file).actions[1];
        let ActionKind::Goal { goal } = &action.kind else {
            panic!("expected GOAL, got {:?}", action.kind);
        };
        assert_eq!(goal.segments.len(), 2);
        assert_eq!(action.timeout.map(DurationLit::millis), Some(180_000));
        assert!(file.uses_goal() && file.uses_ai() && !file.uses_act());
        assert_eq!(
            parse_err("VISIT /\nGOAL \"a\" \"b\"\n").message,
            "expected end of line"
        );
        assert_eq!(parse_err("VISIT /\nGOAL\n").message, "expected a goal");
    }

    #[test]
    fn judge_needs_a_page() {
        assert_eq!(
            parse_err("HTTP GET /x\nJUDGE \"fine\"\n").message,
            "JUDGE checks the page; an HTTP entry has no page"
        );
        assert!(
            parse_err("VISIT /\nJUDGE\n")
                .message
                .starts_with("expected")
        );
    }

    #[test]
    fn malformed_extract_schemas_are_parse_errors() {
        assert_eq!(
            parse_err("VISIT /\nEXTRACT n \"x\"\n{\"type\": \"{{kind}}\"}\n").message,
            "an EXTRACT schema cannot contain `{{ }}`"
        );
        assert!(
            parse_err("VISIT /\nEXTRACT n \"x\"\n{\"type\": }\n")
                .message
                .starts_with("invalid EXTRACT schema")
        );
        assert_eq!(
            parse_err("VISIT /\nEXTRACT n \"x\"\n[1]\n").message,
            "an EXTRACT schema is a JSON object"
        );
        assert_eq!(
            parse_err("VISIT /\nEXTRACT \"x\"\n").message,
            "expected a name"
        );
        assert_eq!(
            parse_err("VISIT /\nEXTRACT n\n").message,
            "expected an instruction"
        );
    }

    #[test]
    fn a_file_needs_at_least_one_entry() {
        let error = parse_err("# only a comment\n");
        assert!(
            error.message.contains("at least one entry"),
            "message: {}",
            error.message
        );
        let error = parse_err("");
        assert!(
            error.message.contains("at least one entry"),
            "message: {}",
            error.message
        );
    }

    #[test]
    fn page_takes_a_value_or_matches_regex() {
        let source = "VISIT /\nPAGE /dashboard\n";
        let file = parse(source);
        let page = only_entry(&file).page.as_ref().expect("PAGE should parse");
        let PageCheck::Value(value) = &page.check else {
            panic!("expected a value check");
        };
        assert_eq!(lit(value), "/dashboard");

        let source = "VISIT /\nPAGE matches /orders\\/\\d+/i @5s\n";
        let file = parse(source);
        let page = only_entry(&file).page.as_ref().expect("PAGE should parse");
        let PageCheck::Matches(regex) = &page.check else {
            panic!("expected a matches check");
        };
        assert_eq!(regex.pattern, "orders\\/\\d+");
        assert!(regex.flags.ignore_case);
        assert_eq!(
            page.timeout,
            Some(DurationLit {
                amount: 5,
                unit:   DurationUnit::Seconds,
            })
        );
    }

    #[test]
    fn a_second_page_line_is_an_error() {
        let error = parse_err("VISIT /\nPAGE /a\nPAGE /b\n");
        assert!(
            error.message.contains("one PAGE"),
            "message: {}",
            error.message
        );
    }

    #[test]
    fn every_option_key_parses() {
        let source = "[Options]\nbase: https://shop.example.com\nbrowser: firefox\nviewport: 1280x800\nstep-timeout: 5s\nentry-timeout: 90s\nnav-timeout: 500ms\nallow-hosts: example.com *.example.com\ndialogs: accept\nreduced-motion: reduce\nstorage: auth/state.json\nuser-agent: \"Whirl/1 (test)\"\nsetup: sign-in.whirl\nVISIT /\n";
        let file = parse(source);
        assert_eq!(file.options.len(), 12);
        let options: Vec<&FileOption> = file.options.iter().map(|line| &line.option).collect();
        let FileOption::Base(base) = options[0] else {
            panic!("expected base");
        };
        assert_eq!(lit(base), "https://shop.example.com");
        assert_eq!(
            *options[1],
            FileOption::Browser(OptionValue::Literal(BrowserKind::Firefox))
        );
        assert_eq!(
            *options[2],
            FileOption::Viewport(OptionValue::Literal(Viewport {
                width:  1280,
                height: 800,
            }))
        );
        assert_eq!(
            *options[3],
            FileOption::StepTimeout(OptionValue::Literal(DurationLit {
                amount: 5,
                unit:   DurationUnit::Seconds,
            }))
        );
        assert_eq!(
            *options[4],
            FileOption::EntryTimeout(OptionValue::Literal(DurationLit {
                amount: 90,
                unit:   DurationUnit::Seconds,
            }))
        );
        assert_eq!(
            *options[5],
            FileOption::NavTimeout(OptionValue::Literal(DurationLit {
                amount: 500,
                unit:   DurationUnit::Milliseconds,
            }))
        );
        let FileOption::AllowHosts(hosts) = options[6] else {
            panic!("expected allow-hosts");
        };
        let hosts: Vec<String> = hosts.iter().map(lit).collect();
        assert_eq!(hosts, vec![
            "example.com".to_owned(),
            "*.example.com".to_owned()
        ]);
        assert_eq!(
            *options[7],
            FileOption::Dialogs(OptionValue::Literal(DialogPolicy::Accept))
        );
        assert_eq!(
            *options[8],
            FileOption::ReducedMotion(OptionValue::Literal(ReducedMotion::Reduce))
        );
        let FileOption::Storage(storage) = options[9] else {
            panic!("expected storage");
        };
        assert_eq!(lit(storage), "auth/state.json");
        let FileOption::UserAgent(user_agent) = options[10] else {
            panic!("expected user-agent");
        };
        assert_eq!(lit(user_agent), "Whirl/1 (test)");
        let FileOption::Setup(setup) = options[11] else {
            panic!("expected setup");
        };
        assert_eq!(lit(setup), "sign-in.whirl");
    }

    #[test]
    fn setup_variable_references_parse_as_their_own_segment() {
        let ActionKind::Visit { url } = action_kind("VISIT /u/{{setup.user_id}}") else {
            panic!("expected VISIT");
        };
        assert_eq!(url.segments, vec![
            ValueSegment::Literal("/u/".to_owned()),
            ValueSegment::SetupVar("user_id".to_owned()),
        ]);
        let error = parse_err("VISIT /\nVISIT {{setup.bad-name}}\n");
        assert!(
            error.message.contains("invalid variable reference"),
            "message: {}",
            error.message
        );
    }

    #[test]
    fn unknown_option_key_is_a_parse_error() {
        let error = parse_err("[Options]\nspeed: fast\nVISIT /\n");
        assert_eq!(error.message, "unknown option key `speed`");
        assert_eq!((error.line, error.column, error.len), (2, 1, 5));
        assert!(error.expected.iter().any(|alt| alt == "base"));
    }

    #[test]
    fn invalid_option_values_are_parse_errors() {
        let error = parse_err("[Options]\nbrowser: chrome\nVISIT /\n");
        assert!(error.expected.iter().any(|alt| alt == "chromium"));
        let error = parse_err("[Options]\nviewport: wide\nVISIT /\n");
        assert!(
            error
                .expected
                .iter()
                .any(|alt| alt.contains("WIDTHxHEIGHT"))
        );
        let error = parse_err("[Options]\nstep-timeout: 10\nVISIT /\n");
        assert!(error.expected.iter().any(|alt| alt.contains("duration")));
    }

    #[test]
    fn interpolated_option_values_defer_shape_validation() {
        let source = "[Options]\nbrowser: {{env.BROWSER}}\nVISIT /\n";
        let file = parse(source);
        let FileOption::Browser(OptionValue::Interpolated(value)) = &file.options[0].option else {
            panic!("expected a deferred browser value");
        };
        assert_eq!(value.segments, vec![ValueSegment::EnvVar(
            "BROWSER".to_owned()
        )]);
    }

    #[test]
    fn options_after_the_first_entry_are_an_error() {
        let error = parse_err("VISIT /\n[Options]\nbase: x\n");
        assert!(
            error.message.contains("[Options]"),
            "message: {}",
            error.message
        );
    }

    #[test]
    fn unknown_sections_are_an_error() {
        let error = parse_err("VISIT /\n[Wibble]\n");
        assert_eq!(error.message, "unknown section `[Wibble]`");
        assert_eq!(error.expected, ["[Options]"]);
    }

    #[test]
    fn timeout_suffix_overrides_the_step_timeout() {
        let source = "VISIT /\nCLICK \"Generate report\" @60s\n";
        let file = parse(source);
        let action = &only_entry(&file).actions[1];
        assert_eq!(
            action.timeout,
            Some(DurationLit {
                amount: 60,
                unit:   DurationUnit::Seconds,
            })
        );
        assert_eq!(action.text, "CLICK \"Generate report\" @60s");
    }

    #[test]
    fn quoted_at_duration_is_an_ordinary_value() {
        let ActionKind::Fill { target: _, value } = action_kind("FILL \"Delay\" \"@60s\"") else {
            panic!("expected FILL");
        };
        assert_eq!(lit(&value), "@60s");
        assert!(value.quoted);
    }

    #[test]
    fn a_bare_at_token_that_is_not_a_duration_is_no_timeout() {
        // `@60x` is not "of the form `@duration`" (SPEC 3.1), so it is an
        // ordinary token; here it is an extra token after CLICK's locator.
        let error = parse_err("VISIT /\nCLICK \"Go\" @60x\n");
        assert!(
            !error.message.contains("step-timeout"),
            "message: {}",
            error.message
        );
    }

    #[test]
    fn locator_chains_join_segments_with_arrows() {
        let source = "VISIT /\nCAPTURE link: testid:result-card >> nth:-1 >> link:* attr:href\n";
        let file = parse(source);
        let capture = &only_entry(&file).captures().collect::<Vec<_>>()[0];
        let Subject::Element { locator, extractor } = &capture.subject else {
            panic!("expected an element source");
        };
        assert_eq!(locator.segments.len(), 3);
        assert!(matches!(locator.segments[1].kind, SegmentKind::Nth(-1)));
        assert_eq!(*extractor, Extractor::Attr("href".to_owned()));
    }

    #[test]
    fn role_takes_an_optional_accessible_name() {
        let ActionKind::Click { target, .. } = action_kind("CLICK button:\"Sign in\"") else {
            panic!("expected CLICK");
        };
        let SegmentKind::Role {
            substring,
            role,
            name,
        } = &target.segments[0].kind
        else {
            panic!("expected a role segment");
        };
        assert!(!substring);
        assert_eq!(role, "button");
        assert_eq!(
            lit(name.as_ref().expect("role name should be present")),
            "Sign in"
        );

        let ActionKind::Click { target, .. } = action_kind("CLICK button:*") else {
            panic!("expected CLICK");
        };
        assert!(matches!(&target.segments[0].kind, SegmentKind::Role {
            name: None,
            ..
        }));
    }

    #[test]
    fn substring_prefix_variants_parse() {
        let ActionKind::Click { target, .. } = action_kind("CLICK text:~\"Added\"") else {
            panic!("expected CLICK");
        };
        let SegmentKind::TextEngine {
            prefix,
            substring,
            value,
        } = &target.segments[0].kind
        else {
            panic!("expected a text engine segment");
        };
        assert_eq!(*prefix, TextPrefix::Text);
        assert!(substring);
        assert_eq!(lit(value), "Added");
    }

    #[test]
    fn a_tilde_after_the_colon_matches_by_substring() {
        let role = |line: &str| {
            let ActionKind::Click { target, .. } = action_kind(line) else {
                panic!("expected CLICK");
            };
            let SegmentKind::Role {
                substring, name, ..
            } = &target.segments[0].kind
            else {
                panic!("expected a role segment");
            };
            (*substring, name.as_ref().map(lit))
        };
        assert_eq!(role("CLICK button:~Sign"), (true, Some("Sign".to_owned())));
        assert_eq!(
            role("CLICK button:~\"Sign in\""),
            (true, Some("Sign in".to_owned()))
        );
        assert_eq!(role("CLICK button:~~x"), (true, Some("~x".to_owned())));
        assert_eq!(role("CLICK button:\"~x\""), (false, Some("~x".to_owned())));
        assert_eq!(role("CLICK button:~\"*\""), (true, Some("*".to_owned())));
        for (line, message) in [
            ("CLICK button:~*", "`button:~` needs a name"),
            ("CLICK label:~", "`label:` needs a value after `~`"),
            ("CLICK button~:Sign", "unknown prefix `button~:`"),
        ] {
            let error = parse_err(&format!("VISIT /\n{line}\n"));
            assert!(
                error.message.starts_with(message),
                "{line}: {}",
                error.message
            );
        }
        // Prefixes without a substring form keep the `~` in the value.
        let ActionKind::Click { target, .. } = action_kind("CLICK css:~x") else {
            panic!("expected CLICK");
        };
        let SegmentKind::Css(value) = &target.segments[0].kind else {
            panic!("expected a css segment");
        };
        assert_eq!(lit(value), "~x");
    }

    #[test]
    fn quoted_css_prefix_is_a_plain_value() {
        let ActionKind::Click { target, .. } = action_kind("CLICK \"css:foo\"") else {
            panic!("expected CLICK");
        };
        assert_eq!(default_segment_text(&target), "css:foo");
    }

    #[test]
    fn bare_css_prefix_is_a_css_segment() {
        let ActionKind::Click { target, .. } = action_kind("CLICK css:\"ul > li\"") else {
            panic!("expected CLICK");
        };
        let SegmentKind::Css(value) = &target.segments[0].kind else {
            panic!("expected a css segment");
        };
        assert_eq!(lit(value), "ul > li");
    }

    #[test]
    fn nth_takes_a_signed_zero_based_index() {
        let error = parse_err("VISIT /\nCLICK testid:card >> nth:+1\n");
        assert_eq!(error.message, "expected an index after `nth:`");
        assert_eq!(error.line, 2);
        let file = parse("VISIT /\nCLICK testid:card >> nth:0\n");
        let ActionKind::Click { target, .. } = &only_entry(&file).actions[1].kind else {
            panic!("expected CLICK");
        };
        assert!(matches!(target.segments[1].kind, SegmentKind::Nth(0)));
    }

    #[test]
    fn nth_may_not_be_the_first_segment() {
        let error = parse_err("VISIT /\nCLICK nth:2\n");
        assert!(
            error.message.contains("first segment"),
            "message: {}",
            error.message
        );
    }

    #[test]
    fn unprefixed_segment_in_asserts_is_a_parse_error() {
        let error = parse_err("VISIT /\nASSERT \"Welcome\" visible\n");
        assert!(
            error.message.contains("unprefixed"),
            "message: {}",
            error.message
        );
        assert_eq!(error.line, 2);
        assert_eq!(error.column, 8);
    }

    #[test]
    fn unprefixed_segment_in_captures_is_a_parse_error() {
        let error = parse_err("VISIT /\nCAPTURE name: \"Welcome\" text\n");
        assert!(
            error.message.contains("unprefixed"),
            "message: {}",
            error.message
        );
    }

    #[test]
    fn interpolation_splits_quoted_and_bare_values() {
        let ActionKind::Fill { target: _, value } = action_kind("FILL \"Email\" \"hi {{name}}!\"")
        else {
            panic!("expected FILL");
        };
        assert_eq!(value.segments, vec![
            ValueSegment::Literal("hi ".to_owned()),
            ValueSegment::Var("name".to_owned()),
            ValueSegment::Literal("!".to_owned()),
        ]);

        let ActionKind::Visit { url } = action_kind("VISIT {{first_product}}") else {
            panic!("expected VISIT");
        };
        assert_eq!(url.segments, vec![ValueSegment::Var(
            "first_product".to_owned()
        )]);
    }

    #[test]
    fn env_references_parse_to_env_segments() {
        let ActionKind::Fill { target: _, value } =
            action_kind("FILL \"Password\" {{env.TEST_PASSWORD}}")
        else {
            panic!("expected FILL");
        };
        assert_eq!(value.segments, vec![ValueSegment::EnvVar(
            "TEST_PASSWORD".to_owned()
        )]);
    }

    #[test]
    fn escaped_braces_make_a_literal() {
        let ActionKind::Fill { target: _, value } = action_kind("FILL \"Note\" \"a \\{{b\"") else {
            panic!("expected FILL");
        };
        assert_eq!(value.segments, vec![ValueSegment::Literal(
            "a {{b".to_owned()
        )]);
    }

    #[test]
    fn string_escapes_apply() {
        let ActionKind::Eval { script } =
            action_kind("EVAL \"line\\n\\ttab \\\"q\\\" \\\\ \\u{1F600}\"")
        else {
            panic!("expected EVAL");
        };
        assert_eq!(lit(&script), "line\n\ttab \"q\" \\ \u{1F600}");
    }

    #[test]
    fn invalid_escape_is_a_parse_error() {
        let error = parse_err("VISIT /\nEVAL \"bad \\q escape\"\n");
        assert!(
            error.message.contains("invalid escape"),
            "message: {}",
            error.message
        );
        assert!(error.expected.iter().any(|alt| alt.contains("\\n")));
    }

    #[test]
    fn visit_parses_a_url() {
        let file = parse("VISIT /login\n");
        let entry = only_entry(&file);
        let ActionKind::Visit { url } = &entry.actions[0].kind else {
            panic!("expected VISIT");
        };
        assert_eq!(lit(url), "/login");
    }

    #[test]
    fn click_uses_the_text_default_engine() {
        let kind = action_kind("CLICK \"Add to cart\"");
        let ActionKind::Click { target, .. } = &kind else {
            panic!("expected CLICK");
        };
        assert_eq!(default_segment_text(target), "Add to cart");
        assert_eq!(kind.default_engine(), Some(DefaultEngine::Text));
    }

    #[test]
    fn right_and_middle_click_press_their_buttons_with_the_text_engine() {
        for (line, expected) in [
            ("RIGHTCLICK \"report.pdf\"", MouseButton::Right),
            ("MIDDLECLICK link:Docs", MouseButton::Middle),
            ("CLICK Save", MouseButton::Left),
        ] {
            let kind = action_kind(line);
            let ActionKind::Click { button, .. } = &kind else {
                panic!("expected a click for {line}");
            };
            assert_eq!(*button, expected, "{line}");
            assert_eq!(kind.default_engine(), Some(DefaultEngine::Text));
        }
    }

    #[test]
    fn dblclick_and_hover_take_locators() {
        assert!(matches!(
            action_kind("DBLCLICK text:Row"),
            ActionKind::Dblclick { .. }
        ));
        assert!(matches!(
            action_kind("HOVER testid:menu"),
            ActionKind::Hover { .. }
        ));
    }

    #[test]
    fn fill_takes_locator_and_value() {
        let kind = action_kind("FILL \"Email\" alice@example.com");
        let ActionKind::Fill { target, value } = &kind else {
            panic!("expected FILL");
        };
        assert_eq!(default_segment_text(target), "Email");
        assert_eq!(lit(value), "alice@example.com");
        assert_eq!(kind.default_engine(), Some(DefaultEngine::Label));
    }

    #[test]
    fn type_takes_locator_and_text_with_the_label_engine() {
        let kind = action_kind("TYPE \"Enter verification code\" 424242");
        let ActionKind::Type { target, text } = &kind else {
            panic!("expected TYPE");
        };
        assert_eq!(default_segment_text(target), "Enter verification code");
        assert_eq!(lit(text), "424242");
        assert_eq!(kind.default_engine(), Some(DefaultEngine::Label));
    }

    #[test]
    fn type_with_one_argument_is_an_error() {
        let error = parse_err("VISIT /\nTYPE 424242\n");
        assert!(
            error.message.contains("expected the text to type"),
            "message: {}",
            error.message
        );
    }

    #[test]
    fn store_takes_a_scope_a_key_and_a_value() {
        let kind = action_kind("STORE local onboarding:done \"yes\"");
        let ActionKind::Store { scope, key, value } = &kind else {
            panic!("expected STORE");
        };
        assert_eq!(*scope, StoreScope::Local);
        assert_eq!(lit(key), "onboarding:done");
        assert_eq!(lit(value), "yes");
        assert_eq!(kind.default_engine(), None);
    }

    #[test]
    fn store_accepts_the_session_and_cookie_scopes() {
        let ActionKind::Store { scope, .. } = action_kind("STORE session draft \"hi\"") else {
            panic!("expected STORE");
        };
        assert_eq!(scope, StoreScope::Session);
        let ActionKind::Store { scope, .. } = action_kind("STORE cookie chat_version v1") else {
            panic!("expected STORE");
        };
        assert_eq!(scope, StoreScope::Cookie);
    }

    #[test]
    fn store_rejects_an_unknown_scope_and_a_missing_value() {
        let error = parse_err("VISIT /\nSTORE global flag on\n");
        assert_eq!(error.message, "expected a storage scope");
        assert!(error.expected.iter().any(|alt| alt == "local"));
        assert!(error.expected.iter().any(|alt| alt == "cookie"));
        let error = parse_err("VISIT /\nSTORE local flag\n");
        assert_eq!(error.message, "expected a value");
        let error = parse_err("VISIT /\nSTORE local flag on extra\n");
        assert_eq!(error.message, "expected end of line");
    }

    #[test]
    fn act_takes_an_optional_prefixed_scope() {
        let ActionKind::Act { scope, instruction } = action_kind("ACT \"click Buy\"") else {
            panic!("expected ACT");
        };
        assert!(scope.is_none());
        assert_eq!(lit(&instruction), "click Buy");

        let ActionKind::Act { scope, instruction } =
            action_kind("ACT css:form >> group:* \"click Buy\"")
        else {
            panic!("expected ACT");
        };
        assert_eq!(scope.expect("a scope").segments.len(), 2);
        assert_eq!(lit(&instruction), "click Buy");

        let error = parse_err("VISIT /\nACT main \"click Buy\"\n");
        assert_eq!(error.line, 2);
    }

    #[test]
    fn press_with_one_argument_treats_it_as_the_key() {
        let ActionKind::Press { target, key } = action_kind("PRESS Enter") else {
            panic!("expected PRESS");
        };
        assert!(target.is_none());
        assert_eq!(lit(&key), "Enter");
    }

    #[test]
    fn press_with_two_arguments_takes_a_locator_first() {
        let ActionKind::Press { target, key } = action_kind("PRESS \"Comment\" Control+A") else {
            panic!("expected PRESS");
        };
        let target = target.expect("PRESS with two arguments has a locator");
        assert_eq!(default_segment_text(&target), "Comment");
        assert_eq!(lit(&key), "Control+A");
    }

    #[test]
    fn check_uncheck_and_select_parse() {
        assert!(matches!(
            action_kind("CHECK label:Terms"),
            ActionKind::Check { .. }
        ));
        assert!(matches!(
            action_kind("UNCHECK label:Terms"),
            ActionKind::Uncheck { .. }
        ));
        let ActionKind::Select { target: _, option } =
            action_kind("SELECT \"Country\" \"Iceland\"")
        else {
            panic!("expected SELECT");
        };
        assert_eq!(lit(&option), "Iceland");
    }

    #[test]
    fn upload_strips_the_file_prefix() {
        let ActionKind::Upload { target: _, path } =
            action_kind("UPLOAD \"Avatar\" file:images/me.png")
        else {
            panic!("expected UPLOAD");
        };
        assert_eq!(lit(&path), "images/me.png");
    }

    #[test]
    fn upload_requires_the_file_prefix() {
        let error = parse_err("VISIT /\nUPLOAD \"Avatar\" images/me.png\n");
        assert!(
            error.message.contains("file:"),
            "message: {}",
            error.message
        );
    }

    #[test]
    fn drop_takes_a_text_locator_and_strips_the_file_prefix() {
        let kind = action_kind("DROP \"Drop files here\" file:reports/q3.csv");
        let ActionKind::Drop { target, path } = &kind else {
            panic!("expected DROP");
        };
        assert_eq!(default_segment_text(target), "Drop files here");
        assert_eq!(lit(path), "reports/q3.csv");
        assert_eq!(kind.default_engine(), Some(DefaultEngine::Text));

        // A quoted path takes spaces; a prefixed locator keeps its engine.
        let ActionKind::Drop { target, path } =
            action_kind("DROP testid:dropzone file:\"my report.csv\"")
        else {
            panic!("expected DROP");
        };
        assert!(matches!(target.segments[0].kind, SegmentKind::TestId(_)));
        assert_eq!(lit(&path), "my report.csv");
    }

    #[test]
    fn drop_reads_a_quoted_file_prefix_as_a_value() {
        // A quoted "file:..." before the path is the zone's text.
        let ActionKind::Drop { target, path } = action_kind("DROP \"file:zone\" file:a.csv") else {
            panic!("expected DROP");
        };
        assert_eq!(default_segment_text(&target), "file:zone");
        assert_eq!(lit(&path), "a.csv");

        // A quoted "file:..." at the end is not the path.
        for (line, message) in [
            (
                "DROP \"Drop files here\" \"file:a.csv\"",
                "expected a `file:` path",
            ),
            ("DROP \"Drop files here\" a.csv", "expected a `file:` path"),
            ("DROP file:a.csv", "unknown prefix `file:`"),
            (
                "DROP \"Drop files here\" file:",
                "expected a path after `file:`",
            ),
            ("DROP", "expected a locator"),
        ] {
            let error = parse_err(&format!("VISIT /\n{line}\n"));
            assert!(error.message.contains(message), "{line}: {}", error.message);
        }
    }

    #[test]
    fn drag_splits_its_locators_at_a_bare_to() {
        let kind = action_kind("DRAG \"Write spec\" to testid:done");
        let ActionKind::Drag { source, target } = &kind else {
            panic!("expected DRAG");
        };
        assert_eq!(default_segment_text(source), "Write spec");
        assert!(matches!(target.segments[0].kind, SegmentKind::TestId(_)));
        assert_eq!(kind.default_engine(), Some(DefaultEngine::Text));

        // A role takes its name before `to`; a quoted "to" is text.
        let ActionKind::Drag { source, target } =
            action_kind("DRAG listitem:\"to\" to region:Done")
        else {
            panic!("expected DRAG");
        };
        let SegmentKind::Role {
            name: Some(name), ..
        } = &source.segments[0].kind
        else {
            panic!("expected a named role");
        };
        assert_eq!(lit(name), "to");
        assert!(matches!(target.segments[0].kind, SegmentKind::Role {
            name: Some(_),
            ..
        }));
        let ActionKind::Drag { source, .. } = action_kind("DRAG \"to\" to Done") else {
            panic!("expected DRAG");
        };
        assert_eq!(default_segment_text(&source), "to");
    }

    #[test]
    fn drag_reads_to_after_its_first_locator() {
        for (line, message) in [
            ("DRAG \"Write spec\" testid:done", "expected `to`"),
            ("DRAG to testid:done", "expected `to`"),
            ("DRAG \"Write spec\" to", "expected a locator"),
            ("DRAG a to b to c", "expected end of line"),
        ] {
            let error = parse_err(&format!("VISIT /\n{line}\n"));
            assert!(error.message.contains(message), "{line}: {}", error.message);
        }
        // Reading left to right, a first `to` is text to match.
        let ActionKind::Drag { source, .. } = action_kind("DRAG to to testid:done") else {
            panic!("expected DRAG");
        };
        assert_eq!(default_segment_text(&source), "to");
    }

    #[test]
    fn scroll_reads_its_motion_from_the_end_of_the_line() {
        let ActionKind::ScrollIntoView { target } = action_kind("SCROLL testid:load-more") else {
            panic!("expected SCROLL into view");
        };
        assert!(matches!(target.segments[0].kind, SegmentKind::TestId(_)));
        assert_eq!(action_kind("SCROLL down"), ActionKind::Scroll {
            target: None,
            motion: ScrollMotion::Chunk(ScrollDirection::Down),
        });
        let ActionKind::Scroll {
            target: Some(target),
            motion,
        } = action_kind("SCROLL dialog:Filters left")
        else {
            panic!("expected SCROLL with a locator");
        };
        assert!(matches!(target.segments[0].kind, SegmentKind::Role {
            name: Some(_),
            ..
        }));
        assert_eq!(motion, ScrollMotion::Chunk(ScrollDirection::Left));
        let ActionKind::Scroll { target, motion } = action_kind("SCROLL to 33.5%") else {
            panic!("expected SCROLL to a position");
        };
        assert!(target.is_none());
        let ScrollMotion::To(percent) = motion else {
            panic!("expected a position");
        };
        assert_eq!(percent.to_string(), "33.5%");
        let ActionKind::ScrollIntoView { target } = action_kind("SCROLL \"down\"") else {
            panic!("expected a quoted direction to be text");
        };
        assert_eq!(default_segment_text(&target), "down");
        assert_eq!(
            action_kind("SCROLL \"to\" up").default_engine(),
            Some(DefaultEngine::Text)
        );
    }

    #[test]
    fn scroll_rejects_a_bad_percent_or_a_stray_to() {
        for (line, message) in [
            ("SCROLL", "expected what to scroll"),
            ("SCROLL to 150%", "from 0% to 100%"),
            ("SCROLL to fifty", "from 0% to 100%"),
            ("SCROLL to", "expected a percent"),
            ("SCROLL a to b down", "from 0% to 100%"),
        ] {
            let error = parse_err(&format!("VISIT /\n{line}\n"));
            assert!(error.message.contains(message), "{line}: {}", error.message);
        }
    }

    #[test]
    fn a_percent_is_digits_from_0_to_100() {
        for text in ["0%", "50%", "100%", "33.5%", "100.0%"] {
            assert!(Percent::parse(text).is_some(), "{text}");
        }
        for text in ["100.5%", "-1%", "50", ".5%", "5.%", "1e2%", "%"] {
            assert!(Percent::parse(text).is_none(), "{text}");
        }
    }

    #[test]
    fn snapshot_options_have_separate_scopes_and_keep_lines() {
        let file = parse(
            "[Options]\nsnapshot-mask: testid:clock\nsnapshot-mask: frame:iframe >> css:.price\nsnapshot-max-diff: 0.1%\nsnapshot-pixel-threshold: 2e-1\nVISIT /\nSNAPSHOT one @2s\n# keep this\nsnapshot-mask: none\nsnapshot-max-diff: 0\nSNAPSHOT two\nPAGE /\n",
        );
        assert_eq!(file.options.len(), 4);
        let actions = &file.entries[0].actions;
        let ActionKind::Snapshot { options, .. } = &actions[1].kind else {
            panic!("snapshot");
        };
        assert_eq!(options.len(), 2);
        assert_eq!(options[0].line, 9);
        assert!(matches!(options[0].option, SnapshotOption::Mask(None)));
        assert!(actions[1].text.contains("\nsnapshot-max-diff: 0"));
        assert!(
            matches!(&actions[2].kind, ActionKind::Snapshot { options, .. } if options.is_empty())
        );
    }

    #[test]
    fn snapshot_options_validate_both_scopes() {
        for invalid in [
            "snapshot-unknown: 1",
            "snapshot-mask: bare",
            "snapshot-mask: \"none\"",
            "snapshot-mask: {{mask}}",
            "snapshot-mask: css:x @1s",
            "snapshot-mask: none\nsnapshot-mask: css:x",
            "snapshot-mask: css:x\nsnapshot-mask: none",
            "snapshot-mask: none\nsnapshot-mask: none",
            "snapshot-max-diff: 1\nsnapshot-max-diff: 2",
            "snapshot-pixel-threshold: 0\nsnapshot-pixel-threshold: 1",
            "snapshot-max-diff: -1",
            "snapshot-max-diff: +1",
            "snapshot-max-diff: 1.5",
            "snapshot-max-diff: 1e2",
            "snapshot-max-diff: 9007199254740992",
            "snapshot-max-diff: 100.1%",
            "snapshot-max-diff: .1%",
            "snapshot-pixel-threshold: NaN",
            "snapshot-pixel-threshold: 1e999",
            "snapshot-pixel-threshold: 1.1",
            "snapshot-pixel-threshold: -0.1",
            "snapshot-pixel-threshold: 20%",
        ] {
            for prefix in ["[Options]\n", "VISIT /\nSNAPSHOT x\n"] {
                parse_err(&format!("{prefix}{invalid}\n"));
            }
        }
        for valid in [
            "snapshot-max-diff: 9007199254740991",
            "snapshot-max-diff: 0",
            "snapshot-max-diff: 100%",
            "snapshot-max-diff: 0.125%",
            "snapshot-pixel-threshold: 0",
            "snapshot-pixel-threshold: 1",
            "snapshot-mask: text:none",
            "snapshot-mask: button:~\"Buy now\" >> nth:0",
            "snapshot-max-diff: {{limit}}",
            "snapshot-pixel-threshold: {{threshold}}",
            "snapshot-mask: testid:{{id}}",
        ] {
            parse(&format!(
                "[Options]\n{valid}\nVISIT /\nSNAPSHOT x\n{valid}\n"
            ));
        }
        for prefix in [
            "VISIT /\nSCREENSHOT x",
            "VISIT /\nSNAPSHOT x\nASSERT url == x",
            "VISIT /\nSNAPSHOT x\nPAGE /",
        ] {
            parse_err(&format!("{prefix}\nsnapshot-max-diff: 1\n"));
        }
    }

    #[test]
    fn screenshot_and_snapshot_take_names() {
        let ActionKind::Screenshot { name } = action_kind("SCREENSHOT overview") else {
            panic!("expected SCREENSHOT");
        };
        assert_eq!(name.text, "overview");
        let ActionKind::Snapshot { name, .. } = action_kind("SNAPSHOT cart_page") else {
            panic!("expected SNAPSHOT");
        };
        assert_eq!(name.text, "cart_page");
    }

    #[test]
    fn snapshot_takes_an_optional_explicit_target_after_its_name() {
        let file = parse(
            "VISIT /\nSNAPSHOT page\nSNAPSHOT cart testid:cart @10s\nsnapshot-max-diff: 1\nSNAPSHOT pay frame:\"#pay iframe\" >> button:\"Pay now\" >> nth:0\nSNAPSHOT row testid:{{row}}\n",
        );
        let actions = &file.entries[0].actions;
        let ActionKind::Snapshot { name, target, .. } = &actions[1].kind else {
            panic!("snapshot");
        };
        assert_eq!(name.text, "page");
        assert!(target.is_none());
        let ActionKind::Snapshot {
            name,
            target: Some(target),
            options,
        } = &actions[2].kind
        else {
            panic!("element snapshot");
        };
        assert_eq!(name.text, "cart");
        assert_eq!(target.segments.len(), 1);
        assert!(
            matches!(&target.segments[0].kind, SegmentKind::TestId(value) if lit(value) == "cart")
        );
        assert_eq!(actions[2].timeout.map(DurationLit::millis), Some(10_000));
        assert_eq!(options.len(), 1);
        let ActionKind::Snapshot {
            target: Some(target),
            ..
        } = &actions[3].kind
        else {
            panic!("frame snapshot");
        };
        assert_eq!(target.segments.len(), 3);
        assert!(matches!(target.segments[0].kind, SegmentKind::Frame(_)));
        assert!(matches!(target.segments[2].kind, SegmentKind::Nth(0)));
        assert!(matches!(&actions[4].kind, ActionKind::Snapshot {
            target: Some(_),
            ..
        }));
    }

    #[test]
    fn snapshot_targets_need_a_name_and_explicit_segments() {
        for invalid in [
            "SNAPSHOT",
            "SNAPSHOT testid:cart",
            "SNAPSHOT \"cart\" testid:cart",
            "SNAPSHOT cart cart",
            "SNAPSHOT cart \"Add to cart\"",
            "SNAPSHOT cart {{target}}",
            "SNAPSHOT cart nth:0",
            "SNAPSHOT cart testid:cart >>",
            "SNAPSHOT cart testid:cart testid:total",
            "SNAPSHOT cart frame:iframe",
            "SNAPSHOT cart testid:cart >> total",
        ] {
            parse_err(&format!("VISIT /\n{invalid}\n"));
        }
    }

    #[test]
    fn screenshot_rejects_a_quoted_name() {
        let error = parse_err("VISIT /\nSCREENSHOT \"overview\"\n");
        assert!(error.message.contains("name"), "message: {}", error.message);
    }

    #[test]
    fn eval_takes_a_script_value() {
        let ActionKind::Eval { script } = action_kind("EVAL \"foo(); bar();\"") else {
            panic!("expected EVAL");
        };
        assert_eq!(lit(&script), "foo(); bar();");
    }
}

#[cfg(test)]
mod frame_tests {
    use std::path::Path;

    use super::parse_file;

    fn parse(source: &str) -> Result<super::File, super::ParseError> {
        parse_file(Path::new("frames.whirl"), source)
    }

    #[test]
    fn rejects_a_frame_without_an_inner_element_in_actions_asserts_and_captures() {
        assert!(parse("VISIT /\nCLICK frame:iframe\n").is_err());
        assert!(parse("VISIT /\nASSERT frame:iframe >> nth:1 visible\n").is_err());
        assert!(parse("VISIT /\nCAPTURE x: frame:iframe text\n").is_err());
    }
}
