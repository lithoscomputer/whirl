//! Line-oriented parser for `.whirl` files (SPEC sections 3-10, 16, 17).
//!
//! [`parse_file`] parses one file and stops at that file's first error.
//! [`parse_files`] parses many files and reports every file's error, so
//! `whirl check` can surface all broken files in one pass (SPEC 13).

use std::fmt::{self, Write as _};
use std::iter::Peekable;
use std::mem;
use std::path::{Path, PathBuf};
use std::vec::IntoIter;

use crate::lang::ast::{
    Action, ActionKind, Assert, AssertBody, BrowserKind, Capture, CaptureSource, Comment,
    DialogPolicy, DurationLit, DurationUnit, Entry, Extractor, File, FileOption, Ident, Locator,
    LocatorSegment, NumOp, OptionLine, OptionValue, Page, PageCheck, Regex, RegexFlags,
    SegmentKind, Span, StateCheck, StrCheck, TextPrefix, Value, ValueSegment, ValueSource,
    Viewport,
};

/// A parse diagnostic (SPEC 16): file, line, column, the source line, a
/// caret under the offending token, and the expected alternatives.
#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
#[error("{}", self.render())]
pub struct ParseError {
    pub path:        PathBuf,
    /// 1-based line of the offending token.
    pub line:        u32,
    /// 1-based character column of the offending token.
    pub column:      u32,
    /// Length of the offending token in characters (caret width).
    pub len:         u32,
    /// The full source line, without its line ending.
    pub source_line: String,
    pub message:     String,
    /// Expected alternatives, possibly empty.
    pub expected:    Vec<String>,
}

impl ParseError {
    /// Renders the diagnostic: location and message, the source line, a
    /// caret under the offending token, and the expected alternatives.
    pub fn render(&self) -> String {
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

/// A diagnostic local to one line; the parser adds file and line context.
struct LineError {
    column:   u32,
    len:      u32,
    message:  String,
    expected: Vec<String>,
}

impl LineError {
    fn new(span: Span, message: impl Into<String>) -> Self {
        Self {
            column:   span.column,
            len:      span.len,
            message:  message.into(),
            expected: Vec::new(),
        }
    }

    fn expecting<S: fmt::Display>(mut self, expected: impl IntoIterator<Item = S>) -> Self {
        self.expected = expected.into_iter().map(|item| item.to_string()).collect();
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

/// Parses a number literal: a non-negative decimal integer (SPEC 3.1).
fn parse_number(text: &str) -> Option<u64> {
    if text.is_empty() || !text.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    text.parse().ok()
}

/// Parses a duration literal: integer plus `ms` or `s` (SPEC 3.1).
pub(crate) fn parse_duration(text: &str) -> Option<DurationLit> {
    let (digits, unit) = if let Some(digits) = text.strip_suffix("ms") {
        (digits, DurationUnit::Milliseconds)
    } else {
        let digits = text.strip_suffix('s')?;
        (digits, DurationUnit::Seconds)
    };
    parse_number(digits).map(|amount| DurationLit { amount, unit })
}

/// One whitespace-free run of source: adjacent bare runs and quoted
/// strings form a single token (`placeholder:"Search products"`).
#[derive(Clone, Debug)]
struct RawToken {
    parts: Vec<RawPart>,
    span:  Span,
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

    /// Converts the token to a value, splitting bare interpolation.
    fn into_value(self) -> Result<Value, LineError> {
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
    } else if is_ident(&name) {
        ValueSegment::Var(name)
    } else {
        return Err(
            LineError::new(span, format!("invalid variable reference `{{{{{name}}}}}`"))
                .expecting(["a variable name like {{name}} or {{env.NAME}}"]),
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

    /// The trailing comment at the cursor, if the rest of the line is one.
    fn take_comment(&mut self) -> Option<Comment> {
        self.skip_ws();
        if self.peek() != Some('#') {
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
        matches!(self.peek(), None | Some('#'))
    }

    fn span_from(&self, start: usize) -> Span {
        Span {
            line:   self.line_no,
            column: u32::try_from(start + 1).unwrap_or(u32::MAX),
            len:    u32::try_from(self.pos - start).unwrap_or(u32::MAX),
        }
    }

    /// Scans the next token, or `None` at the end of the line or at a
    /// comment. Bare runs end at whitespace, `"`, or `#` (SPEC 3.1);
    /// adjacent runs and quoted strings join into one token.
    fn next_token(&mut self) -> Result<Option<RawToken>, LineError> {
        if self.at_line_end() {
            return Ok(None);
        }
        let start = self.pos;
        let mut parts = Vec::new();
        loop {
            match self.peek() {
                Some('"') => parts.push(self.scan_quoted()?),
                Some(ch) if !ch.is_whitespace() && ch != '#' => parts.push(self.scan_bare()),
                _ => break,
            }
        }
        Ok(Some(RawToken {
            parts,
            span: self.span_from(start),
        }))
    }

    fn scan_bare(&mut self) -> RawPart {
        let start = self.pos;
        let column = self.column();
        while let Some(ch) = self.peek() {
            if ch.is_whitespace() || ch == '"' || ch == '#' {
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
            if ch.is_whitespace() || ch == '#' {
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

const SEGMENT_PREFIXES: [&str; 9] = [
    "role:",
    "label:",
    "placeholder:",
    "text:",
    "alt:",
    "title:",
    "testid:",
    "css:",
    "nth:",
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
    })
}

/// Parses one locator segment from a token (SPEC 6, 17). A `Role`
/// segment's accessible name may follow in the next token; the locator
/// loops attach it.
fn parse_segment(
    token: RawToken,
    allow_default: bool,
    is_first: bool,
) -> Result<LocatorSegment, LineError> {
    let span = token.span;
    let head = match token.parts.first() {
        Some(RawPart::Bare { text, .. }) => text.clone(),
        _ => String::new(),
    };
    let value_after = |prefix: &str| -> Result<Value, LineError> {
        strip_prefix_token(token.clone(), prefix.len())
            .ok_or_else(|| {
                LineError::new(span, format!("`{prefix}` needs a value")).expecting(["a value"])
            })
            .and_then(RawToken::into_value)
    };
    for (name, prefix) in TEXT_PREFIXES {
        for (marker, substring) in [(format!("{name}:"), false), (format!("{name}~:"), true)] {
            if head.starts_with(&marker) {
                let value = value_after(&marker)?;
                let kind = SegmentKind::TextEngine {
                    prefix,
                    substring,
                    value,
                };
                return Ok(LocatorSegment { kind, span });
            }
        }
    }
    for (marker, substring) in [("role:", false), ("role~:", true)] {
        if let Some(role) = head.strip_prefix(marker) {
            let role = role.to_owned();
            if !is_ident(&role) || token.parts.len() > 1 {
                return Err(LineError::new(span, "expected a role type after `role:`")
                    .expecting(["a role type like button or heading"]));
            }
            let kind = SegmentKind::Role {
                substring,
                role,
                name: None,
            };
            return Ok(LocatorSegment { kind, span });
        }
    }
    if head.starts_with("testid:") {
        let value = value_after("testid:")?;
        return Ok(LocatorSegment {
            kind: SegmentKind::TestId(value),
            span,
        });
    }
    if head.starts_with("css:") {
        let value = value_after("css:")?;
        return Ok(LocatorSegment {
            kind: SegmentKind::Css(value),
            span,
        });
    }
    if let Some(rest) = head.strip_prefix("nth:") {
        if token.parts.len() > 1 {
            return Err(LineError::new(span, "expected a number after `nth:`")
                .expecting(["a 1-based index"]));
        }
        let Some(index) = parse_number(rest) else {
            return Err(LineError::new(span, "expected a number after `nth:`")
                .expecting(["a 1-based index"]));
        };
        if index == 0 {
            return Err(
                LineError::new(span, "`nth:` is 1-based; `nth:0` is not allowed")
                    .expecting(["an index of 1 or more"]),
            );
        }
        if is_first {
            return Err(
                LineError::new(span, "`nth:` may not be the first segment of a locator")
                    .expecting(["a locator segment before `nth:`"]),
            );
        }
        return Ok(LocatorSegment {
            kind: SegmentKind::Nth(index),
            span,
        });
    }
    if allow_default {
        let value = token.into_value()?;
        return Ok(LocatorSegment {
            kind: SegmentKind::Default(value),
            span,
        });
    }
    Err(LineError::new(
        span,
        "unprefixed locator segments are only allowed in actions; use a prefix here",
    )
    .expecting(SEGMENT_PREFIXES))
}

fn locator_span(first: Span, last: Span) -> Span {
    Span {
        line:   first.line,
        column: first.column,
        len:    (last.column + last.len).saturating_sub(first.column),
    }
}

/// Builds a locator from a fixed token list (action lines, SPEC 6):
/// segments joined by `>>`, with a role segment's optional accessible
/// name taken from the following token.
fn build_locator(
    tokens: Vec<RawToken>,
    allow_default: bool,
    missing_at: Span,
) -> Result<Locator, LineError> {
    let mut iter = tokens.into_iter().peekable();
    let Some(first) = iter.next() else {
        return Err(LineError::new(missing_at, "expected a locator").expecting(["a locator"]));
    };
    let first_span = first.span;
    let mut last_span = first.span;
    let mut segments = vec![parse_segment(first, allow_default, true)?];
    loop {
        attach_role_name(&mut segments, &mut iter, &mut last_span)?;
        let Some(sep) = iter.next() else {
            break;
        };
        if sep.bare_single() != Some(">>") {
            return Err(
                LineError::new(sep.span, "expected `>>` between locator segments")
                    .expecting([">>"]),
            );
        }
        let Some(next) = iter.next() else {
            return Err(
                LineError::new(sep.span, "expected a locator segment after `>>`")
                    .expecting(["a locator segment"]),
            );
        };
        last_span = next.span;
        segments.push(parse_segment(next, allow_default, false)?);
    }
    Ok(Locator {
        segments,
        span: locator_span(first_span, last_span),
    })
}

/// When the last segment is a nameless `role:`, consumes the next token
/// as its accessible name unless the token is the `>>` separator.
fn attach_role_name(
    segments: &mut [LocatorSegment],
    iter: &mut Peekable<IntoIter<RawToken>>,
    last_span: &mut Span,
) -> Result<(), LineError> {
    let Some(segment) = segments.last_mut() else {
        return Ok(());
    };
    let SegmentKind::Role {
        name: name @ None, ..
    } = &mut segment.kind
    else {
        return Ok(());
    };
    let takes_name = iter
        .peek()
        .is_some_and(|token| token.bare_single() != Some(">>"));
    if takes_name {
        let token = iter.next().expect("peeked token is present");
        *last_span = token.span;
        segment.span = locator_span(segment.span, token.span);
        *name = Some(token.into_value()?);
    }
    Ok(())
}

/// Strips a final `@duration` step-timeout suffix (SPEC 12). Only a bare
/// token counts: a quoted `"@60s"` is an ordinary value (SPEC 3.1). A bare
/// `@` token that is not a valid duration (`@zzz`) is not "of the form
/// `@duration`", so it stays an ordinary value too.
fn split_timeout(tokens: &mut Vec<RawToken>) -> Option<DurationLit> {
    let duration = tokens
        .last()?
        .bare_single()
        .and_then(|text| text.strip_prefix('@'))
        .and_then(parse_duration)?;
    tokens.pop();
    Some(duration)
}

const ACTION_KEYWORDS: [&str; 13] = [
    "VISIT",
    "CLICK",
    "DBLCLICK",
    "FILL",
    "PRESS",
    "CHECK",
    "UNCHECK",
    "SELECT",
    "HOVER",
    "UPLOAD",
    "SCREENSHOT",
    "SNAPSHOT",
    "EVAL",
];

fn one_value(mut tokens: Vec<RawToken>, keyword_span: Span) -> Result<Value, LineError> {
    if tokens.len() > 1 {
        let extra = &tokens[1];
        return Err(LineError::new(extra.span, "expected end of line")
            .expecting(["a single value", "@duration"]));
    }
    let Some(token) = tokens.pop() else {
        return Err(LineError::new(keyword_span, "expected a value").expecting(["a value"]));
    };
    token.into_value()
}

/// Splits `locator value` tokens: the final token is the value, everything
/// before it is the locator (SPEC 7).
fn locator_and_value(
    mut tokens: Vec<RawToken>,
    keyword_span: Span,
    allow_default: bool,
) -> Result<(Locator, Value), LineError> {
    let Some(value_token) = tokens.pop() else {
        return Err(
            LineError::new(keyword_span, "expected a locator and a value")
                .expecting(["a locator", "a value"]),
        );
    };
    if tokens.is_empty() {
        return Err(
            LineError::new(value_token.span, "expected a locator and a value")
                .expecting(["a locator before this value"]),
        );
    }
    let locator = build_locator(tokens, allow_default, keyword_span)?;
    Ok((locator, value_token.into_value()?))
}

fn parse_name(mut tokens: Vec<RawToken>, keyword_span: Span) -> Result<Ident, LineError> {
    let expected = ["a name matching [A-Za-z_][A-Za-z0-9_]*"];
    if tokens.len() > 1 {
        return Err(LineError::new(tokens[1].span, "expected end of line").expecting(expected));
    }
    let Some(token) = tokens.pop() else {
        return Err(LineError::new(keyword_span, "expected a name").expecting(expected));
    };
    match token.bare_single() {
        Some(text) if is_ident(text) => Ok(Ident {
            text: text.to_owned(),
            span: token.span,
        }),
        _ => Err(LineError::new(token.span, "expected a name").expecting(expected)),
    }
}

/// Parses an action line after its keyword (SPEC 7, 17).
fn parse_action_body(
    keyword: &str,
    keyword_span: Span,
    cursor: &mut Cursor,
) -> Result<(ActionKind, Option<DurationLit>), LineError> {
    let mut tokens = Vec::new();
    while let Some(token) = cursor.next_token()? {
        tokens.push(token);
    }
    let timeout = split_timeout(&mut tokens);
    let locator_only = |tokens| build_locator(tokens, true, keyword_span);
    let kind = match keyword {
        "VISIT" => ActionKind::Visit {
            url: one_value(tokens, keyword_span)?,
        },
        "CLICK" => ActionKind::Click {
            target: locator_only(tokens)?,
        },
        "DBLCLICK" => ActionKind::Dblclick {
            target: locator_only(tokens)?,
        },
        "HOVER" => ActionKind::Hover {
            target: locator_only(tokens)?,
        },
        "CHECK" => ActionKind::Check {
            target: locator_only(tokens)?,
        },
        "UNCHECK" => ActionKind::Uncheck {
            target: locator_only(tokens)?,
        },
        "FILL" => {
            let (target, value) = locator_and_value(tokens, keyword_span, true)?;
            ActionKind::Fill { target, value }
        }
        "SELECT" => {
            let (target, option) = locator_and_value(tokens, keyword_span, true)?;
            ActionKind::Select { target, option }
        }
        "PRESS" => parse_press(tokens, keyword_span)?,
        "UPLOAD" => parse_upload(tokens, keyword_span)?,
        "SCREENSHOT" => ActionKind::Screenshot {
            name: parse_name(tokens, keyword_span)?,
        },
        "SNAPSHOT" => ActionKind::Snapshot {
            name: parse_name(tokens, keyword_span)?,
        },
        "EVAL" => ActionKind::Eval {
            script: one_value(tokens, keyword_span)?,
        },
        other => {
            return Err(
                LineError::new(keyword_span, format!("unknown action `{other}`"))
                    .expecting(ACTION_KEYWORDS),
            );
        }
    };
    Ok((kind, timeout))
}

/// `PRESS` with one argument treats it as the key; only with two is the
/// first a locator (SPEC 7).
fn parse_press(tokens: Vec<RawToken>, keyword_span: Span) -> Result<ActionKind, LineError> {
    match tokens.len() {
        0 => Err(LineError::new(keyword_span, "expected a key").expecting(["a key like Enter"])),
        1 => {
            let mut tokens = tokens;
            let key = tokens.pop().expect("one token is present").into_value()?;
            Ok(ActionKind::Press { target: None, key })
        }
        _ => {
            let (target, key) = locator_and_value(tokens, keyword_span, true)?;
            Ok(ActionKind::Press {
                target: Some(target),
                key,
            })
        }
    }
}

/// `UPLOAD locator file:path` — the final value carries the `file:`
/// prefix (SPEC 7). A quoted `"file:..."` is a value, not the prefix.
fn parse_upload(mut tokens: Vec<RawToken>, keyword_span: Span) -> Result<ActionKind, LineError> {
    let Some(file_token) = tokens.pop() else {
        return Err(
            LineError::new(keyword_span, "expected a locator and a `file:` path")
                .expecting(["file:"]),
        );
    };
    let starts_with_file = matches!(
        file_token.parts.first(),
        Some(RawPart::Bare { text, .. }) if text.starts_with("file:")
    );
    if !starts_with_file {
        return Err(LineError::new(file_token.span, "expected a `file:` path").expecting(["file:"]));
    }
    let path = strip_prefix_token(file_token.clone(), "file:".len())
        .ok_or_else(|| {
            LineError::new(file_token.span, "expected a path after `file:`").expecting(["a path"])
        })?
        .into_value()?;
    if tokens.is_empty() {
        return Err(
            LineError::new(keyword_span, "expected a locator before the `file:` path")
                .expecting(["a locator"]),
        );
    }
    let target = build_locator(tokens, true, keyword_span)?;
    Ok(ActionKind::Upload { target, path })
}

/// The position just after a span, for "expected more here" diagnostics.
fn after_span(span: Span) -> Span {
    Span {
        line:   span.line,
        column: span.column + span.len + 1,
        len:    1,
    }
}

/// Scans a locator token by token until a stop keyword ends it (the check
/// in `[Asserts]`, the extractor in `[Captures]`). Returns the locator
/// and the consumed stop token, or `None` when the line ended first.
/// A bare stop keyword after a nameless `role:` segment is the stop, not
/// the accessible name; a quoted name is never a keyword (SPEC 3.1).
fn scan_locator(
    first: RawToken,
    cursor: &mut Cursor,
    is_stop: &dyn Fn(&str) -> bool,
    stop_expected: &[&str],
) -> Result<(Locator, Option<RawToken>), LineError> {
    let first_span = first.span;
    let mut last_span = first.span;
    let mut segments = vec![parse_segment(first, false, true)?];
    loop {
        let span = locator_span(first_span, last_span);
        let Some(token) = cursor.next_token()? else {
            return Ok((Locator { segments, span }, None));
        };
        if let Some(text) = token.bare_single() {
            if is_stop(text) {
                return Ok((Locator { segments, span }, Some(token)));
            }
            if text == ">>" {
                let Some(next) = cursor.next_token()? else {
                    return Err(LineError::new(
                        token.span,
                        "expected a locator segment after `>>`",
                    )
                    .expecting(["a locator segment"]));
                };
                last_span = next.span;
                segments.push(parse_segment(next, false, false)?);
                continue;
            }
        }
        if let Some(LocatorSegment {
            kind: SegmentKind::Role {
                name: name @ None, ..
            },
            span: role_span,
        }) = segments.last_mut()
        {
            *role_span = locator_span(*role_span, token.span);
            last_span = token.span;
            *name = Some(token.into_value()?);
            continue;
        }
        let mut expected = vec![">>"];
        expected.extend_from_slice(stop_expected);
        return Err(
            LineError::new(token.span, "expected `>>` or the end of the locator")
                .expecting(expected),
        );
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

fn is_assert_stop(text: &str) -> bool {
    STATE_CHECKS.iter().any(|(name, _)| *name == text)
        || matches!(text, "text" | "value" | "count")
        || text.starts_with("attr:")
}

const STR_OPS: [&str; 4] = ["==", "!=", "contains", "matches"];

/// Parses a string check's operand: the next token as a value. A final
/// bare `@duration` is the step timeout, never the operand (SPEC 3.1).
fn parse_operand(cursor: &mut Cursor, op_span: Span) -> Result<Value, LineError> {
    let Some(token) = cursor.next_token()? else {
        return Err(LineError::new(after_span(op_span), "expected a value").expecting(["a value"]));
    };
    let is_trailing_timeout = token
        .bare_single()
        .and_then(|text| text.strip_prefix('@'))
        .is_some_and(|rest| parse_duration(rest).is_some())
        && cursor.at_line_end();
    if is_trailing_timeout {
        return Err(LineError::new(
            token.span,
            "a final bare `@duration` is the step timeout; quote it to use it as a value",
        )
        .expecting(["a value"]));
    }
    token.into_value()
}

/// Parses `== value | != value | contains value | matches /re/` (SPEC 9.4).
fn parse_str_check(cursor: &mut Cursor, at: Span) -> Result<StrCheck, LineError> {
    let Some(op) = cursor.next_token()? else {
        return Err(LineError::new(after_span(at), "expected an operator").expecting(STR_OPS));
    };
    match op.bare_single() {
        Some("==") => Ok(StrCheck::Eq(parse_operand(cursor, op.span)?)),
        Some("!=") => Ok(StrCheck::Ne(parse_operand(cursor, op.span)?)),
        Some("contains") => Ok(StrCheck::Contains(parse_operand(cursor, op.span)?)),
        Some("matches") => Ok(StrCheck::Matches(cursor.expect_regex()?)),
        _ => Err(LineError::new(op.span, "expected an operator").expecting(STR_OPS)),
    }
}

const NUM_OPS: [(&str, NumOp); 6] = [
    ("==", NumOp::Eq),
    ("!=", NumOp::Ne),
    ("<", NumOp::Lt),
    ("<=", NumOp::Le),
    (">", NumOp::Gt),
    (">=", NumOp::Ge),
];

/// Parses `numop number` after `count` (SPEC 9.4).
fn parse_count_check(cursor: &mut Cursor, at: Span) -> Result<(NumOp, u64), LineError> {
    let ops = NUM_OPS.map(|(name, _)| name);
    let Some(op_token) = cursor.next_token()? else {
        return Err(LineError::new(after_span(at), "expected a count operator").expecting(ops));
    };
    let op = op_token
        .bare_single()
        .and_then(|text| {
            NUM_OPS
                .iter()
                .find(|(name, _)| *name == text)
                .map(|(_, op)| *op)
        })
        .ok_or_else(|| LineError::new(op_token.span, "expected a count operator").expecting(ops))?;
    let Some(number_token) = cursor.next_token()? else {
        return Err(
            LineError::new(after_span(op_token.span), "expected a number")
                .expecting(["a non-negative integer"]),
        );
    };
    let number = number_token
        .bare_single()
        .and_then(parse_number)
        .ok_or_else(|| {
            LineError::new(number_token.span, "expected a number")
                .expecting(["a non-negative integer"])
        })?;
    Ok((op, number))
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

/// Parses one `[Asserts]` line (SPEC 9, 17).
fn parse_assert_body(
    first: RawToken,
    cursor: &mut Cursor,
) -> Result<(AssertBody, Option<DurationLit>), LineError> {
    let first_span = first.span;
    let body = match first.bare_single() {
        Some("url") => AssertBody::Url(parse_str_check(cursor, first_span)?),
        Some("title") => AssertBody::Title(parse_str_check(cursor, first_span)?),
        _ => {
            let (locator, stop) = scan_locator(first, cursor, &is_assert_stop, &ASSERT_CHECKS)?;
            let Some(stop) = stop else {
                return Err(LineError::new(after_span(locator.span), "expected a check")
                    .expecting(ASSERT_CHECKS));
            };
            let text = stop.bare_single().unwrap_or_default().to_owned();
            if let Some((_, state)) = STATE_CHECKS.iter().find(|(name, _)| *name == text) {
                AssertBody::ElementState {
                    locator,
                    state: *state,
                }
            } else if text == "text" {
                let check = parse_str_check(cursor, stop.span)?;
                AssertBody::ElementValue {
                    locator,
                    source: ValueSource::Text,
                    check,
                }
            } else if text == "value" {
                let check = parse_str_check(cursor, stop.span)?;
                AssertBody::ElementValue {
                    locator,
                    source: ValueSource::Value,
                    check,
                }
            } else if let Some(name) = text.strip_prefix("attr:") {
                if !is_attr_name(name) {
                    return Err(LineError::new(stop.span, "invalid attribute name")
                        .expecting(["a name like aria-expanded or data-state"]));
                }
                let check = parse_str_check(cursor, stop.span)?;
                let source = ValueSource::Attr(name.to_owned());
                AssertBody::ElementValue {
                    locator,
                    source,
                    check,
                }
            } else {
                let (op, count) = parse_count_check(cursor, stop.span)?;
                AssertBody::ElementCount { locator, op, count }
            }
        }
    };
    let timeout = parse_line_timeout(cursor)?;
    Ok((body, timeout))
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
        PageCheck::Matches(cursor.expect_regex()?)
    } else {
        let is_trailing_timeout = token
            .bare_single()
            .and_then(|text| text.strip_prefix('@'))
            .is_some_and(|rest| parse_duration(rest).is_some())
            && cursor.at_line_end();
        if is_trailing_timeout {
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

/// Splits a leading `name:` from a token: the identifier before the first
/// `:` and the remaining token, if anything follows the colon.
fn split_name_colon(token: RawToken) -> Option<(Ident, Option<RawToken>)> {
    let RawPart::Bare { text, .. } = token.parts.first()? else {
        return None;
    };
    let colon = text.find(':')?;
    let name = text[..colon].to_owned();
    if !is_ident(&name) {
        return None;
    }
    let span = Span {
        line:   token.span.line,
        column: token.span.column,
        len:    u32::try_from(colon).unwrap_or(u32::MAX),
    };
    let ident = Ident { text: name, span };
    let rest = strip_prefix_token(token, colon + 1);
    Some((ident, rest))
}

const EXTRACTORS: [&str; 4] = ["text", "value", "count", "attr:NAME"];

fn is_extractor(text: &str) -> bool {
    matches!(text, "text" | "value" | "count") || text.starts_with("attr:")
}

/// Parses one `[Captures]` line after its `name:` head (SPEC 10, 17).
fn parse_capture_body(
    name: Ident,
    rest: Option<RawToken>,
    cursor: &mut Cursor,
) -> Result<Capture, LineError> {
    let first = match rest {
        Some(token) => token,
        None => cursor.next_token()?.ok_or_else(|| {
            LineError::new(after_span(name.span), "expected a capture source").expecting([
                "a locator",
                "url",
                "title",
                "eval",
            ])
        })?,
    };
    let source = match first.bare_single() {
        Some("url") => CaptureSource::Url,
        Some("title") => CaptureSource::Title,
        Some("eval") => {
            let script = parse_operand(cursor, first.span)?;
            CaptureSource::Eval(script)
        }
        _ => {
            let (locator, stop) = scan_locator(first, cursor, &is_extractor, &EXTRACTORS)?;
            let Some(stop) = stop else {
                return Err(
                    LineError::new(after_span(locator.span), "expected an extractor")
                        .expecting(EXTRACTORS),
                );
            };
            let text = stop.bare_single().unwrap_or_default();
            let extractor = match text {
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
            CaptureSource::Element { locator, extractor }
        }
    };
    let mut filter = None;
    let mut timeout = None;
    if let Some(token) = cursor.next_token()? {
        if token.bare_single() == Some("regex") {
            filter = Some(cursor.expect_regex()?);
            timeout = parse_line_timeout(cursor)?;
        } else {
            let mut tokens = vec![token];
            timeout = split_timeout(&mut tokens);
            if let Some(extra) = tokens.first() {
                return Err(
                    LineError::new(extra.span, "expected end of line").expecting([
                        "regex /re/",
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
    }
    Ok(Capture {
        name,
        source,
        filter,
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

const OPTION_KEYS: [&str; 9] = [
    "base",
    "browser",
    "viewport",
    "step-timeout",
    "entry-timeout",
    "nav-timeout",
    "allow-hosts",
    "dialogs",
    "storage",
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

fn parse_viewport(text: &str) -> Option<Viewport> {
    let (width, height) = text.split_once('x')?;
    Some(Viewport {
        width:  parse_number(width)?,
        height: parse_number(height)?,
    })
}

/// Parses one `key: value` line in `[Options]` (SPEC 5).
fn parse_option_line(first: RawToken, cursor: &mut Cursor) -> Result<FileOption, LineError> {
    let first_span = first.span;
    let head = match first.parts.first() {
        Some(RawPart::Bare { text, .. }) => text.clone(),
        _ => String::new(),
    };
    let Some(colon) = head.find(':') else {
        return Err(LineError::new(first_span, "expected an option line").expecting(["key: value"]));
    };
    let key = head[..colon].to_owned();
    if !OPTION_KEYS.contains(&key.as_str()) {
        let key_span = Span {
            line:   first_span.line,
            column: first_span.column,
            len:    u32::try_from(colon).unwrap_or(u32::MAX),
        };
        return Err(
            LineError::new(key_span, format!("unknown option key `{key}`")).expecting(OPTION_KEYS),
        );
    }
    let mut values = Vec::new();
    if let Some(rest) = strip_prefix_token(first, colon + 1) {
        values.push(rest.into_value()?);
    }
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
    let duration = |value| option_shape(value, parse_duration, &["a duration like 500ms or 10s"]);
    match key.as_str() {
        "base" => Ok(FileOption::Base(value)),
        "browser" => {
            let parse = |text: &str| match text {
                "chromium" => Some(BrowserKind::Chromium),
                "firefox" => Some(BrowserKind::Firefox),
                "webkit" => Some(BrowserKind::Webkit),
                _ => None,
            };
            Ok(FileOption::Browser(option_shape(value, parse, &[
                "chromium", "firefox", "webkit",
            ])?))
        }
        "viewport" => Ok(FileOption::Viewport(option_shape(
            value,
            parse_viewport,
            &["WIDTHxHEIGHT like 1280x800"],
        )?)),
        "step-timeout" => Ok(FileOption::StepTimeout(duration(value)?)),
        "entry-timeout" => Ok(FileOption::EntryTimeout(duration(value)?)),
        "nav-timeout" => Ok(FileOption::NavTimeout(duration(value)?)),
        "dialogs" => {
            let parse = |text: &str| match text {
                "dismiss" => Some(DialogPolicy::Dismiss),
                "accept" => Some(DialogPolicy::Accept),
                _ => None,
            };
            Ok(FileOption::Dialogs(option_shape(value, parse, &[
                "dismiss", "accept",
            ])?))
        }
        "storage" => Ok(FileOption::Storage(value)),
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
    /// Inside an entry's `[Asserts]` section.
    Asserts,
    /// Inside an entry's `[Captures]` section.
    Captures,
}

struct Parser {
    options:        Vec<OptionLine>,
    entries:        Vec<Entry>,
    comments:       Vec<Comment>,
    current:        Option<Entry>,
    state:          State,
    options_header: Option<u32>,
}

/// Parses one `.whirl` source, stopping at the file's first error.
pub fn parse_file(path: &Path, source: &str) -> Result<File, ParseError> {
    let mut parser = Parser {
        options:        Vec::new(),
        entries:        Vec::new(),
        comments:       Vec::new(),
        current:        None,
        state:          State::Preamble,
        options_header: None,
    };
    let lines: Vec<&str> = source.lines().collect();
    for (index, line) in lines.iter().enumerate() {
        let line_no = u32::try_from(index + 1).unwrap_or(u32::MAX);
        parser
            .parse_line(line, line_no)
            .map_err(|error| into_parse_error(path, error, line_no, line))?;
    }
    if let Some(entry) = parser.current.take() {
        parser.entries.push(entry);
    }
    if parser.entries.is_empty() {
        let line_no = u32::try_from(lines.len().max(1)).unwrap_or(u32::MAX);
        return Err(ParseError {
            path:        path.to_path_buf(),
            line:        line_no,
            column:      1,
            len:         1,
            source_line: lines.last().copied().unwrap_or_default().to_owned(),
            message:     "a file needs at least one entry".to_owned(),
            expected:    vec!["VISIT".to_owned()],
        });
    }
    Ok(File {
        path:           path.to_path_buf(),
        options:        parser.options,
        entries:        parser.entries,
        comments:       parser.comments,
        options_header: parser.options_header,
    })
}

/// Parses many `.whirl` sources and reports every broken file's first
/// error, so one `whirl check` run surfaces them all (SPEC 13, 16).
pub fn parse_files<'a>(
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

fn into_parse_error(path: &Path, error: LineError, line_no: u32, line: &str) -> ParseError {
    ParseError {
        path:        path.to_path_buf(),
        line:        line_no,
        column:      error.column,
        len:         error.len,
        source_line: line.to_owned(),
        message:     error.message,
        expected:    error.expected,
    }
}

impl Parser {
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
            self.handle_step(first, &mut cursor, content_start)?;
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
            "[Asserts]" => match self.state {
                State::Actions | State::AfterPage => {
                    let entry = self
                        .current
                        .as_mut()
                        .expect("the Actions and AfterPage states always have a current entry");
                    entry.asserts_header = Some(span.line);
                    self.state = State::Asserts;
                    Ok(())
                }
                State::Preamble | State::Options => Err(LineError::new(
                    span,
                    "`[Asserts]` must follow an entry's actions",
                )
                .expecting(["an action"])),
                State::Asserts => Err(LineError::new(span, "an entry has one `[Asserts]` section")
                    .expecting(["a check", "an action", "[Captures]"])),
                State::Captures => Err(LineError::new(
                    span,
                    "`[Asserts]` must come before `[Captures]` in an entry",
                )
                .expecting(["a capture", "an action"])),
            },
            "[Captures]" => match self.state {
                State::Actions | State::AfterPage | State::Asserts => {
                    let entry = self
                        .current
                        .as_mut()
                        .expect("the entry-section states always have a current entry");
                    entry.captures_header = Some(span.line);
                    self.state = State::Captures;
                    Ok(())
                }
                State::Preamble | State::Options => Err(LineError::new(
                    span,
                    "`[Captures]` must follow an entry's actions",
                )
                .expecting(["an action"])),
                State::Captures => Err(LineError::new(
                    span,
                    "an entry has one `[Captures]` section",
                )
                .expecting(["a capture", "an action"])),
            },
            other => Err(
                LineError::new(span, format!("unknown section `{other}`")).expecting([
                    "[Options]",
                    "[Asserts]",
                    "[Captures]",
                ]),
            ),
        }
    }

    fn handle_step(
        &mut self,
        first: RawToken,
        cursor: &mut Cursor,
        content_start: usize,
    ) -> Result<(), LineError> {
        let line_no = cursor.line_no;
        let bare = first.bare_single().map(str::to_owned);
        if let Some(keyword) = bare
            .as_deref()
            .filter(|text| ACTION_KEYWORDS.contains(text))
        {
            let keyword = keyword.to_owned();
            let first_span = first.span;
            let (kind, timeout) = parse_action_body(&keyword, first_span, cursor)?;
            let file_first_action = self.entries.is_empty() && self.current.is_none();
            if file_first_action && !matches!(kind, ActionKind::Visit { .. }) {
                return Err(
                    LineError::new(first_span, "the first action in a file must be VISIT")
                        .expecting(["VISIT"]),
                );
            }
            let (text, span) = cursor.content(content_start);
            let action = Action {
                kind,
                timeout,
                line: line_no,
                span,
                text,
            };
            if self.state == State::Actions {
                let entry = self
                    .current
                    .as_mut()
                    .expect("the Actions state always has a current entry");
                entry.actions.push(action);
            } else {
                if let Some(entry) = self.current.take() {
                    self.entries.push(entry);
                }
                self.current = Some(Entry {
                    actions:         vec![action],
                    page:            None,
                    asserts:         Vec::new(),
                    captures:        Vec::new(),
                    asserts_header:  None,
                    captures_header: None,
                });
                self.state = State::Actions;
            }
            return Ok(());
        }
        let is_page = bare.as_deref() == Some("PAGE");
        match self.state {
            State::Preamble => Err(LineError::new(
                first.span,
                "expected `[Options]` or an action; the first action must be VISIT",
            )
            .expecting(["[Options]", "VISIT"])),
            State::Options => {
                let first_span = first.span;
                let option = parse_option_line(first, cursor)?;
                let (_, span) = cursor.content(content_start);
                let _ = first_span;
                self.options.push(OptionLine {
                    option,
                    line: line_no,
                    span,
                });
                Ok(())
            }
            State::Actions if is_page => {
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
            State::Actions => Err(
                LineError::new(first.span, "expected a step line").expecting([
                    "an action",
                    "PAGE",
                    "[Asserts]",
                    "[Captures]",
                ]),
            ),
            State::AfterPage if is_page => Err(LineError::new(
                first.span,
                "an entry has one PAGE line",
            )
            .expecting(["an action", "[Asserts]", "[Captures]"])),
            State::AfterPage => Err(LineError::new(first.span, "expected a step line")
                .expecting(["an action", "[Asserts]", "[Captures]"])),
            State::Asserts if is_page => Err(LineError::new(
                first.span,
                "`PAGE` must come before an entry's `[Asserts]` section",
            )
            .expecting(["a check", "an action"])),
            State::Asserts => {
                let (body, timeout) = parse_assert_body(first, cursor)?;
                let (text, span) = cursor.content(content_start);
                let entry = self
                    .current
                    .as_mut()
                    .expect("the Asserts state always has a current entry");
                entry.asserts.push(Assert {
                    body,
                    timeout,
                    line: line_no,
                    span,
                    text,
                });
                Ok(())
            }
            State::Captures if is_page => Err(LineError::new(
                first.span,
                "`PAGE` must come before an entry's `[Asserts]` and `[Captures]` sections",
            )
            .expecting(["a capture", "an action"])),
            State::Captures => {
                let first_span = first.span;
                let Some((name, rest)) = split_name_colon(first) else {
                    return Err(LineError::new(first_span, "expected a capture line")
                        .expecting(["name: source"]));
                };
                let mut capture = parse_capture_body(name, rest, cursor)?;
                let (text, span) = cursor.content(content_start);
                capture.line = line_no;
                capture.span = span;
                capture.text = text;
                let entry = self
                    .current
                    .as_mut()
                    .expect("the Captures state always has a current entry");
                entry.captures.push(capture);
                Ok(())
            }
        }
    }
}

#[cfg(test)]
mod tests {
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
        let source = format!("VISIT /\n[Asserts]\n{source_tail}\n");
        let file = parse(&source);
        only_entry(&file).asserts[0].body.clone()
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

    #[test]
    fn element_value_checks_parse() {
        let AssertBody::ElementValue { source, check, .. } = only_assert("testid:x text == Alice")
        else {
            panic!("expected a value check");
        };
        assert_eq!(source, ValueSource::Text);
        let StrCheck::Eq(value) = check else {
            panic!("expected ==");
        };
        assert_eq!(lit(&value), "Alice");

        let AssertBody::ElementValue { source, check, .. } =
            only_assert("label:Amount value != \"0\"")
        else {
            panic!("expected a value check");
        };
        assert_eq!(source, ValueSource::Value);
        assert!(matches!(check, StrCheck::Ne(_)));

        let AssertBody::ElementValue { source, check, .. } =
            only_assert("testid:x attr:aria-expanded contains tru")
        else {
            panic!("expected an attr check");
        };
        assert_eq!(source, ValueSource::Attr("aria-expanded".to_owned()));
        assert!(matches!(check, StrCheck::Contains(_)));

        let AssertBody::ElementValue { check, .. } =
            only_assert("testid:order text matches /Order #\\w+/i")
        else {
            panic!("expected a matches check");
        };
        let StrCheck::Matches(regex) = check else {
            panic!("expected matches");
        };
        assert_eq!(regex.pattern, "Order #\\w+");
        assert!(regex.flags.ignore_case);
        assert!(!regex.flags.dot_all);
    }

    #[test]
    fn every_count_operator_parses() {
        let ops = [
            ("==", NumOp::Eq),
            ("!=", NumOp::Ne),
            ("<", NumOp::Lt),
            ("<=", NumOp::Le),
            (">", NumOp::Gt),
            (">=", NumOp::Ge),
        ];
        for (op_text, expected) in ops {
            let body = only_assert(&format!("testid:row count {op_text} 3"));
            let AssertBody::ElementCount { op, count, .. } = body else {
                panic!("expected a count check for {op_text}");
            };
            assert_eq!(op, expected);
            assert_eq!(count, 3);
        }
    }

    #[test]
    fn url_and_title_checks_parse() {
        let AssertBody::Url(StrCheck::Contains(value)) = only_assert("url contains \"q=widget\"")
        else {
            panic!("expected a url check");
        };
        assert_eq!(lit(&value), "q=widget");
        let AssertBody::Title(StrCheck::Eq(value)) = only_assert("title == \"Checkout\"") else {
            panic!("expected a title check");
        };
        assert_eq!(lit(&value), "Checkout");
    }

    #[test]
    fn assert_lines_take_timeout_suffixes() {
        let source = "VISIT /\n[Asserts]\ntestid:x visible @2500ms\n";
        let file = parse(source);
        let assert_line = &only_entry(&file).asserts[0];
        assert_eq!(
            assert_line.timeout,
            Some(DurationLit {
                amount: 2500,
                unit:   DurationUnit::Milliseconds,
            })
        );
    }

    #[test]
    fn a_final_bare_at_token_that_is_not_a_duration_is_a_value() {
        // SPEC 3.1 reserves only tokens "of the form `@duration`"; `@zzz`
        // is an ordinary value in every position, action lines included.
        let source = "VISIT /\nFILL Email @zzz\n";
        let file = parse(source);
        let action = &only_entry(&file).actions[1];
        assert_eq!(action.timeout, None);
        let ActionKind::Fill { value, .. } = &action.kind else {
            panic!("expected FILL");
        };
        assert_eq!(lit(value), "@zzz");
    }

    #[test]
    fn regex_flags_allow_only_i_s_m() {
        let AssertBody::Url(StrCheck::Matches(regex)) = only_assert("url matches /a.b/ism") else {
            panic!("expected matches");
        };
        assert!(regex.flags.ignore_case && regex.flags.dot_all && regex.flags.multiline);

        let error = parse_err("VISIT /\n[Asserts]\nurl matches /a/g\n");
        assert_eq!(error.message, "invalid regex flag `g`");
        assert_eq!(error.expected, vec![
            "i".to_owned(),
            "s".to_owned(),
            "m".to_owned()
        ]);
    }

    #[test]
    fn a_hash_inside_a_regex_is_literal() {
        let AssertBody::Url(StrCheck::Matches(regex)) = only_assert("url matches /a#b/") else {
            panic!("expected matches");
        };
        assert_eq!(regex.pattern, "a#b");
    }

    #[test]
    fn capture_sources_parse() {
        let source = "VISIT /\n[Captures]\nheading: role:heading \"Hi\" text\ninput: label:Email value\nrows: testid:row count\nhref: role:link attr:href\nhere: url\nname: title\nresult: eval \"1 + 1\"\n";
        let file = parse(source);
        let captures = &only_entry(&file).captures;
        assert_eq!(captures.len(), 7);
        assert!(matches!(&captures[0].source, CaptureSource::Element {
            extractor: Extractor::Text,
            ..
        }));
        assert!(matches!(&captures[1].source, CaptureSource::Element {
            extractor: Extractor::Value,
            ..
        }));
        assert!(matches!(&captures[2].source, CaptureSource::Element {
            extractor: Extractor::Count,
            ..
        }));
        let CaptureSource::Element {
            extractor: Extractor::Attr(attr),
            ..
        } = &captures[3].source
        else {
            panic!("expected an attr extractor");
        };
        assert_eq!(attr, "href");
        assert!(matches!(&captures[4].source, CaptureSource::Url));
        assert!(matches!(&captures[5].source, CaptureSource::Title));
        let CaptureSource::Eval(script) = &captures[6].source else {
            panic!("expected an eval source");
        };
        assert_eq!(lit(script), "1 + 1");
        assert_eq!(captures[6].name.text, "result");
    }

    #[test]
    fn capture_regex_filter_and_timeout_parse() {
        let source =
            "VISIT /\n[Captures]\norder_id: testid:confirmation text regex /Order #(\\w+)/ @5s\n";
        let file = parse(source);
        let capture = &only_entry(&file).captures[0];
        let filter = capture.filter.as_ref().expect("regex filter should parse");
        assert_eq!(filter.pattern, "Order #(\\w+)");
        assert_eq!(
            capture.timeout,
            Some(DurationLit {
                amount: 5,
                unit:   DurationUnit::Seconds,
            })
        );
    }

    #[test]
    fn capture_names_must_be_identifiers() {
        let error = parse_err("VISIT /\n[Captures]\n9lives: url\n");
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
CLICK role:button "Sign in"
PAGE /dashboard
[Asserts]
role:heading "Welcome back" visible
testid:user-menu text == Alice

# Find a product.
FILL placeholder:"Search products" widget
PRESS Enter
[Asserts]
url contains "q=widget"
testid:result-card count >= 1
[Captures]
first_product: testid:result-card >> nth:1 >> role:link attr:href

# Add it to the cart.
VISIT {{first_product}}
CLICK "Add to cart"
[Asserts]
testid:cart-badge text == 1
role:alert text contains "Added to cart"
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
        assert_eq!(login.asserts.len(), 2);
        assert!(matches!(login.asserts[0].body, AssertBody::ElementState {
            state: StateCheck::Visible,
            ..
        }));
        assert!(login.captures.is_empty());

        assert_eq!(search.actions.len(), 2);
        assert!(matches!(search.actions[1].kind, ActionKind::Press {
            target: None,
            ..
        }));
        assert_eq!(search.asserts.len(), 2);
        assert!(matches!(
            search.asserts[0].body,
            AssertBody::Url(StrCheck::Contains(_))
        ));
        assert!(matches!(search.asserts[1].body, AssertBody::ElementCount {
            op: NumOp::Ge,
            count: 1,
            ..
        }));
        assert_eq!(search.captures.len(), 1);
        let capture = &search.captures[0];
        assert_eq!(capture.name.text, "first_product");
        let CaptureSource::Element {
            locator,
            extractor: Extractor::Attr(attr),
        } = &capture.source
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
        assert_eq!(cart.asserts.len(), 2);
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
        let source = "VISIT /a\n[Asserts]\nurl == \"/a\"\nCLICK \"Next\"\n";
        let file = parse(source);
        assert_eq!(
            file.entry_display_name(&file.entries[0]),
            "VISIT /a (line 1)"
        );
        assert_eq!(
            file.entry_display_name(&file.entries[1]),
            "CLICK \"Next\" (line 4)"
        );
    }

    #[test]
    fn a_comment_above_an_earlier_step_does_not_name_a_later_entry() {
        let source = "# Log in.\nVISIT /a\n[Asserts]\nurl == \"/a\"\nCLICK \"Next\"\n";
        let file = parse(source);
        assert_eq!(file.entry_display_name(&file.entries[0]), "Log in.");
        assert_eq!(
            file.entry_display_name(&file.entries[1]),
            "CLICK \"Next\" (line 5)"
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
        let error = parse_err("VISIT /\nCLICK testid:card >> nth:0\n");
        let rendered = error.render();
        let lines: Vec<&str> = rendered.lines().collect();
        assert_eq!(lines[1], "  CLICK testid:card >> nth:0");
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
    fn first_action_must_be_visit() {
        let error = parse_err("CLICK \"Go\"\n");
        assert_eq!(error.message, "the first action in a file must be VISIT");
        assert_eq!(error.expected, vec!["VISIT".to_owned()]);
        assert_eq!((error.line, error.column), (1, 1));
    }

    #[test]
    fn later_entries_may_start_with_any_action() {
        let source = "VISIT /\n[Asserts]\nurl == \"/\"\nCLICK \"Next\"\n";
        let file = parse(source);
        assert_eq!(file.entries.len(), 2);
    }

    #[test]
    fn an_action_after_asserts_starts_a_new_entry() {
        let source = "VISIT /a\nCLICK \"One\"\n[Asserts]\ntestid:x visible\nVISIT /b\n[Asserts]\ntestid:y visible\n";
        let file = parse(source);
        assert_eq!(file.entries.len(), 2);
        assert_eq!(file.entries[0].actions.len(), 2);
        assert_eq!(file.entries[0].asserts.len(), 1);
        assert_eq!(file.entries[1].actions.len(), 1);
        assert_eq!(file.entries[1].asserts.len(), 1);
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
        let source = "VISIT /a\n[Captures]\nhere: url\nCLICK \"Next\"\n";
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
        let source = "[Options]\nbase: https://shop.example.com\nbrowser: firefox\nviewport: 1280x800\nstep-timeout: 5s\nentry-timeout: 90s\nnav-timeout: 500ms\nallow-hosts: example.com *.example.com\ndialogs: accept\nstorage: auth/state.json\nVISIT /\n";
        let file = parse(source);
        assert_eq!(file.options.len(), 9);
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
        let FileOption::Storage(storage) = options[8] else {
            panic!("expected storage");
        };
        assert_eq!(lit(storage), "auth/state.json");
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
        assert_eq!(error.expected.len(), 3);
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
        let source =
            "VISIT /\n[Captures]\nlink: testid:result-card >> nth:1 >> role:link attr:href\n";
        let file = parse(source);
        let capture = &only_entry(&file).captures[0];
        let CaptureSource::Element { locator, extractor } = &capture.source else {
            panic!("expected an element source");
        };
        assert_eq!(locator.segments.len(), 3);
        assert!(matches!(locator.segments[1].kind, SegmentKind::Nth(1)));
        assert_eq!(*extractor, Extractor::Attr("href".to_owned()));
    }

    #[test]
    fn role_takes_an_optional_accessible_name() {
        let ActionKind::Click { target } = action_kind("CLICK role:button \"Sign in\"") else {
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

        let ActionKind::Click { target } = action_kind("CLICK role:button") else {
            panic!("expected CLICK");
        };
        assert!(matches!(&target.segments[0].kind, SegmentKind::Role {
            name: None,
            ..
        }));
    }

    #[test]
    fn substring_prefix_variants_parse() {
        let ActionKind::Click { target } = action_kind("CLICK text~:\"Added\"") else {
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
    fn quoted_css_prefix_is_a_plain_value() {
        let ActionKind::Click { target } = action_kind("CLICK \"css:foo\"") else {
            panic!("expected CLICK");
        };
        assert_eq!(default_segment_text(&target), "css:foo");
    }

    #[test]
    fn bare_css_prefix_is_a_css_segment() {
        let ActionKind::Click { target } = action_kind("CLICK css:\"ul > li\"") else {
            panic!("expected CLICK");
        };
        let SegmentKind::Css(value) = &target.segments[0].kind else {
            panic!("expected a css segment");
        };
        assert_eq!(lit(value), "ul > li");
    }

    #[test]
    fn nth_zero_is_a_parse_error() {
        let error = parse_err("VISIT /\nCLICK testid:card >> nth:0\n");
        assert!(
            error.message.contains("1-based"),
            "message: {}",
            error.message
        );
        assert_eq!(error.line, 2);
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
        let error = parse_err("VISIT /\n[Asserts]\n\"Welcome\" visible\n");
        assert!(
            error.message.contains("unprefixed"),
            "message: {}",
            error.message
        );
        assert_eq!(error.line, 3);
        assert_eq!(error.column, 1);
    }

    #[test]
    fn unprefixed_segment_in_captures_is_a_parse_error() {
        let error = parse_err("VISIT /\n[Captures]\nname: \"Welcome\" text\n");
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
        let ActionKind::Click { target } = &kind else {
            panic!("expected CLICK");
        };
        assert_eq!(default_segment_text(target), "Add to cart");
        assert_eq!(kind.default_engine(), Some(DefaultEngine::Text));
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
    fn screenshot_and_snapshot_take_names() {
        let ActionKind::Screenshot { name } = action_kind("SCREENSHOT overview") else {
            panic!("expected SCREENSHOT");
        };
        assert_eq!(name.text, "overview");
        let ActionKind::Snapshot { name } = action_kind("SNAPSHOT cart_page") else {
            panic!("expected SNAPSHOT");
        };
        assert_eq!(name.text, "cart_page");
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
