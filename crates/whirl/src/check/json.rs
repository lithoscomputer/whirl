//! JSON for checks: a strict RFC 8259 parser that keeps every number's
//! exact text (SPEC 9.3), and JSONPath queries (SPEC 9.5).
//!
//! Whirl does not turn on serde_json's `arbitrary_precision` feature,
//! which would change number handling for every crate in the build (ADR
//! `evaluate-checks-in-rust` §1.4). This parser builds the exact tree.

use std::fmt;

use serde_json_path::{JsonPath, PathElement};

use super::filter::{Missing, Step};
use super::number::Number;
use super::value::Value;

/// Nesting deeper than this is an error, so hostile input cannot exhaust
/// the stack. serde_json uses the same limit.
const MAX_DEPTH: usize = 128;

/// A JSON syntax error with its byte offset.
#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
#[error("invalid JSON at byte {offset}: {reason}")]
pub(crate) struct JsonError {
    pub(crate) offset: usize,
    pub(crate) reason: JsonErrorReason,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum JsonErrorReason {
    UnexpectedEnd,
    UnexpectedCharacter,
    InvalidNumber,
    InvalidEscape,
    InvalidUnicode,
    ControlCharacter,
    TrailingCharacters,
    TooDeep,
}

impl fmt::Display for JsonErrorReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::UnexpectedEnd => "unexpected end of input",
            Self::UnexpectedCharacter => "unexpected character",
            Self::InvalidNumber => "invalid number",
            Self::InvalidEscape => "invalid escape",
            Self::InvalidUnicode => "invalid unicode escape",
            Self::ControlCharacter => "control character in string",
            Self::TrailingCharacters => "trailing characters",
            Self::TooDeep => "nesting too deep",
        })
    }
}

/// Parses one JSON text into an exact [`Value`] tree.
pub(crate) fn parse(text: &str) -> Result<Value, JsonError> {
    let mut parser = Parser {
        text,
        bytes: text.as_bytes(),
        pos: 0,
    };
    parser.skip_whitespace();
    let value = parser.value(0)?;
    parser.skip_whitespace();
    if parser.pos < parser.bytes.len() {
        return Err(parser.error(JsonErrorReason::TrailingCharacters));
    }
    Ok(value)
}

struct Parser<'a> {
    text:  &'a str,
    bytes: &'a [u8],
    pos:   usize,
}

impl Parser<'_> {
    fn error(&self, reason: JsonErrorReason) -> JsonError {
        JsonError {
            offset: self.pos,
            reason,
        }
    }

    fn peek(&self) -> Option<u8> {
        self.bytes.get(self.pos).copied()
    }

    fn skip_whitespace(&mut self) {
        while matches!(self.peek(), Some(b' ' | b'\t' | b'\n' | b'\r')) {
            self.pos += 1;
        }
    }

    fn expect_literal(&mut self, literal: &str) -> Result<(), JsonError> {
        if self.text[self.pos..].starts_with(literal) {
            self.pos += literal.len();
            Ok(())
        } else if self.text.len() - self.pos < literal.len()
            && literal.starts_with(&self.text[self.pos..])
        {
            Err(JsonError {
                offset: self.text.len(),
                reason: JsonErrorReason::UnexpectedEnd,
            })
        } else {
            Err(self.error(JsonErrorReason::UnexpectedCharacter))
        }
    }

    fn value(&mut self, depth: usize) -> Result<Value, JsonError> {
        match self.peek() {
            None => Err(self.error(JsonErrorReason::UnexpectedEnd)),
            Some(b'{') => self.object(depth),
            Some(b'[') => self.array(depth),
            Some(b'"') => Ok(Value::String(self.string()?)),
            Some(b't') => self.expect_literal("true").map(|()| Value::Bool(true)),
            Some(b'f') => self.expect_literal("false").map(|()| Value::Bool(false)),
            Some(b'n') => self.expect_literal("null").map(|()| Value::Null),
            Some(b'-' | b'0'..=b'9') => self.number(),
            Some(_) => Err(self.error(JsonErrorReason::UnexpectedCharacter)),
        }
    }

    fn enter(&self, depth: usize) -> Result<usize, JsonError> {
        if depth >= MAX_DEPTH {
            Err(self.error(JsonErrorReason::TooDeep))
        } else {
            Ok(depth + 1)
        }
    }

    fn object(&mut self, depth: usize) -> Result<Value, JsonError> {
        let depth = self.enter(depth)?;
        self.pos += 1;
        let mut members: Vec<(String, Value)> = Vec::new();
        self.skip_whitespace();
        if self.peek() == Some(b'}') {
            self.pos += 1;
            return Ok(Value::Object(members));
        }
        loop {
            self.skip_whitespace();
            if self.peek() != Some(b'"') {
                return Err(self.end_or(JsonErrorReason::UnexpectedCharacter));
            }
            let key = self.string()?;
            self.skip_whitespace();
            if self.peek() != Some(b':') {
                return Err(self.end_or(JsonErrorReason::UnexpectedCharacter));
            }
            self.pos += 1;
            self.skip_whitespace();
            let value = self.value(depth)?;
            match members.iter_mut().find(|(existing, _)| *existing == key) {
                Some((_, slot)) => *slot = value,
                None => members.push((key, value)),
            }
            self.skip_whitespace();
            match self.peek() {
                Some(b',') => self.pos += 1,
                Some(b'}') => {
                    self.pos += 1;
                    return Ok(Value::Object(members));
                }
                _ => return Err(self.end_or(JsonErrorReason::UnexpectedCharacter)),
            }
        }
    }

    fn array(&mut self, depth: usize) -> Result<Value, JsonError> {
        let depth = self.enter(depth)?;
        self.pos += 1;
        let mut items = Vec::new();
        self.skip_whitespace();
        if self.peek() == Some(b']') {
            self.pos += 1;
            return Ok(Value::List(items));
        }
        loop {
            self.skip_whitespace();
            items.push(self.value(depth)?);
            self.skip_whitespace();
            match self.peek() {
                Some(b',') => self.pos += 1,
                Some(b']') => {
                    self.pos += 1;
                    return Ok(Value::List(items));
                }
                _ => return Err(self.end_or(JsonErrorReason::UnexpectedCharacter)),
            }
        }
    }

    fn end_or(&self, reason: JsonErrorReason) -> JsonError {
        if self.pos >= self.bytes.len() {
            self.error(JsonErrorReason::UnexpectedEnd)
        } else {
            self.error(reason)
        }
    }

    fn number(&mut self) -> Result<Value, JsonError> {
        let start = self.pos;
        while matches!(
            self.peek(),
            Some(b'-' | b'+' | b'.' | b'e' | b'E' | b'0'..=b'9')
        ) {
            self.pos += 1;
        }
        Number::parse(&self.text[start..self.pos])
            .map(Value::Number)
            .ok_or(JsonError {
                offset: start,
                reason: JsonErrorReason::InvalidNumber,
            })
    }

    fn string(&mut self) -> Result<String, JsonError> {
        self.pos += 1;
        let mut out = String::new();
        loop {
            let run = self.bytes[self.pos..]
                .iter()
                .take_while(|byte| !matches!(byte, b'"' | b'\\') && **byte >= 0x20)
                .count();
            out.push_str(&self.text[self.pos..self.pos + run]);
            self.pos += run;
            match self.peek() {
                None => return Err(self.error(JsonErrorReason::UnexpectedEnd)),
                Some(b'"') => {
                    self.pos += 1;
                    return Ok(out);
                }
                Some(b'\\') => {
                    self.pos += 1;
                    self.escape(&mut out)?;
                }
                Some(_) => return Err(self.error(JsonErrorReason::ControlCharacter)),
            }
        }
    }

    fn escape(&mut self, out: &mut String) -> Result<(), JsonError> {
        let Some(byte) = self.peek() else {
            return Err(self.error(JsonErrorReason::UnexpectedEnd));
        };
        self.pos += 1;
        match byte {
            b'"' => out.push('"'),
            b'\\' => out.push('\\'),
            b'/' => out.push('/'),
            b'b' => out.push('\u{8}'),
            b'f' => out.push('\u{c}'),
            b'n' => out.push('\n'),
            b'r' => out.push('\r'),
            b't' => out.push('\t'),
            b'u' => {
                let first = self.hex4()?;
                let ch = if (0xD800..0xDC00).contains(&first) {
                    if !self.text[self.pos..].starts_with("\\u") {
                        return Err(self.error(JsonErrorReason::InvalidUnicode));
                    }
                    self.pos += 2;
                    let second = self.hex4()?;
                    if !(0xDC00..0xE000).contains(&second) {
                        return Err(self.error(JsonErrorReason::InvalidUnicode));
                    }
                    0x10000 + ((first - 0xD800) << 10) + (second - 0xDC00)
                } else if (0xDC00..0xE000).contains(&first) {
                    return Err(self.error(JsonErrorReason::InvalidUnicode));
                } else {
                    first
                };
                out.push(char::from_u32(ch).ok_or(self.error(JsonErrorReason::InvalidUnicode))?);
            }
            _ => {
                self.pos -= 1;
                return Err(self.error(JsonErrorReason::InvalidEscape));
            }
        }
        Ok(())
    }

    fn hex4(&mut self) -> Result<u32, JsonError> {
        let Some(digits) = self.text.get(self.pos..self.pos + 4) else {
            return Err(self.error(JsonErrorReason::UnexpectedEnd));
        };
        let value = u32::from_str_radix(digits, 16)
            .ok()
            .filter(|_| digits.bytes().all(|byte| byte.is_ascii_hexdigit()))
            .ok_or(self.error(JsonErrorReason::InvalidUnicode))?;
        self.pos += 4;
        Ok(value)
    }
}

/// A parsed RFC 9535 JSONPath query (SPEC 9.5).
#[derive(Clone, Debug)]
pub(crate) struct JsonQuery {
    text:     String,
    path:     JsonPath,
    singular: bool,
}

impl JsonQuery {
    /// Parses a query such as `$.items[0].id`.
    pub(crate) fn parse(text: &str) -> Result<Self, String> {
        let path =
            JsonPath::parse(text).map_err(|error| format!("invalid JSONPath {text}: {error}"))?;
        Ok(Self {
            text: text.to_owned(),
            path,
            singular: is_singular(text),
        })
    }

    /// The query as written.
    pub(crate) fn text(&self) -> &str {
        &self.text
    }

    /// Runs the query. A string input is parsed as JSON first. A singular
    /// query gives one value or a missing value; any other query gives a
    /// list of every match.
    pub(crate) fn apply(&self, input: Value) -> Result<Step, String> {
        let exact = match input {
            Value::String(text) => parse(&text).map_err(|error| error.to_string())?,
            value @ (Value::List(_) | Value::Object(_)) => value,
            other => return Err(format!("cannot take {}", other.value_type())),
        };
        let loose = to_serde(&exact);
        let located = self.path.query_located(&loose);
        let mut matches = Vec::new();
        for node in located.iter() {
            let found = lookup(&exact, node.location().iter())
                .ok_or_else(|| format!("JSONPath {} returned an unknown location", self.text))?;
            matches.push(found.clone());
        }
        if !self.singular {
            return Ok(Step::Value(Value::List(matches)));
        }
        Ok(match matches.into_iter().next() {
            Some(value) => Step::Value(value),
            None => Step::Missing(Missing::NoJsonMatch(self.text.clone())),
        })
    }
}

/// Follows a normalized path through the exact tree.
fn lookup<'v, 'p>(
    root: &'v Value,
    mut path: impl Iterator<Item = &'p PathElement<'p>>,
) -> Option<&'v Value> {
    path.try_fold(root, |current, element| match (current, element) {
        (Value::Object(members), PathElement::Name(name)) => members
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value),
        (Value::List(items), PathElement::Index(index)) => items.get(*index),
        _ => None,
    })
}

/// The exact tree as an ordinary `serde_json::Value`, for running queries.
/// Numbers become `i64`, `u64`, or `f64`, as serde_json holds them;
/// Whirl reads the exact values back by location.
fn to_serde(value: &Value) -> serde_json::Value {
    match value {
        Value::String(text) => serde_json::Value::String(text.clone()),
        Value::Number(number) => number_to_serde(number),
        Value::Bool(value) => serde_json::Value::Bool(*value),
        Value::List(items) => serde_json::Value::Array(items.iter().map(to_serde).collect()),
        Value::Object(members) => serde_json::Value::Object(
            members
                .iter()
                .map(|(key, value)| (key.clone(), to_serde(value)))
                .collect(),
        ),
        Value::Null | Value::Bytes(_) | Value::Date(_) | Value::NodeSet(_) => {
            serde_json::Value::Null
        }
    }
}

fn number_to_serde(number: &Number) -> serde_json::Value {
    let text = number.text();
    if !number.is_float() {
        if let Ok(value) = text.parse::<i64>() {
            return serde_json::Value::from(value);
        }
        if let Ok(value) = text.parse::<u64>() {
            return serde_json::Value::from(value);
        }
    }
    text.parse::<f64>()
        .ok()
        .and_then(serde_json::Number::from_f64)
        .map_or(serde_json::Value::Null, serde_json::Value::Number)
}

/// True for an RFC 9535 singular query (section 2.3.5.1): only name and
/// index selectors, with no wildcard, slice, filter, or descendant
/// segment. RFC 9535 allows only singular queries in a filter comparison,
/// and the parser enforces that, so the comparison form decides.
fn is_singular(query: &str) -> bool {
    JsonPath::parse(&format!("$[?{query} == null]")).is_ok()
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;

    fn parsed(text: &str) -> Value {
        parse(text).unwrap_or_else(|error| panic!("{text}: {error}"))
    }

    #[test]
    fn keeps_exact_number_text() {
        let value = parsed(r#"{"id":1234567890123456789,"p":1.0,"e":1e3,"n":-0.5}"#);
        assert_eq!(
            value.to_json().as_deref(),
            Some(r#"{"id":1234567890123456789,"p":1.0,"e":1e3,"n":-0.5}"#)
        );
    }

    #[test]
    fn keeps_key_order_and_takes_the_last_duplicate() {
        let value = parsed(r#"{"b":1,"a":2,"b":3}"#);
        assert_eq!(value.to_json().as_deref(), Some(r#"{"b":3,"a":2}"#));
    }

    #[test]
    fn decodes_escapes_and_surrogate_pairs() {
        let value = parsed(r#"["a\"b\\c\/d\n", "\u00e9\ud83d\ude00"]"#);
        let Value::List(items) = value else {
            panic!("a list");
        };
        assert!(matches!(&items[0], Value::String(text) if text == "a\"b\\c/d\n"));
        assert!(matches!(&items[1], Value::String(text) if text == "é😀"));
    }

    #[test]
    fn rejects_invalid_json() {
        for (text, reason) in [
            ("", JsonErrorReason::UnexpectedEnd),
            ("[1,]", JsonErrorReason::UnexpectedCharacter),
            ("{\"a\" 1}", JsonErrorReason::UnexpectedCharacter),
            ("01", JsonErrorReason::InvalidNumber),
            ("\"\\x\"", JsonErrorReason::InvalidEscape),
            ("\"\\ud800\"", JsonErrorReason::InvalidUnicode),
            ("\"a\nb\"", JsonErrorReason::ControlCharacter),
            ("1 2", JsonErrorReason::TrailingCharacters),
            ("tru", JsonErrorReason::UnexpectedEnd),
            ("nul!", JsonErrorReason::UnexpectedCharacter),
        ] {
            assert_eq!(
                parse(text).err().map(|error| error.reason),
                Some(reason),
                "{text:?}"
            );
        }
    }

    #[test]
    fn limits_nesting_depth() {
        let deep = format!("{}{}", "[".repeat(200), "]".repeat(200));
        assert_eq!(
            parse(&deep).err().map(|error| error.reason),
            Some(JsonErrorReason::TooDeep)
        );
    }

    fn query(path: &str, document: &str) -> Step {
        JsonQuery::parse(path)
            .expect("valid query")
            .apply(Value::String(document.to_owned()))
            .expect("query runs")
    }

    fn json_of(step: Step) -> String {
        match step {
            Step::Value(value) => value.to_json().expect("JSON value"),
            Step::Missing(missing) => format!("missing: {missing}"),
        }
    }

    const DOC: &str = r#"{"a":1,"id":1234567890123456789,"items":[{"id":1.0,"sku":"A","price":5},{"id":2,"sku":"B","price":15},{"id":3e0,"sku":"C","price":7}],"a/b":{"~key":"x"}}"#;

    #[test]
    fn singular_queries_give_one_exact_value() {
        assert_eq!(json_of(query("$.id", DOC)), "1234567890123456789");
        assert_eq!(json_of(query("$.items[0].id", DOC)), "1.0");
        assert_eq!(json_of(query("$.items[-1].id", DOC)), "3e0");
        assert_eq!(json_of(query("$['a/b']['~key']", DOC)), r#""x""#);
        assert_eq!(json_of(query("$", DOC)), DOC);
        assert_eq!(
            json_of(query("$.missing", DOC)),
            "missing: JSONPath $.missing selects nothing"
        );
    }

    #[test]
    fn other_queries_give_lists() {
        assert_eq!(json_of(query("$.items[*].sku", DOC)), r#"["A","B","C"]"#);
        assert_eq!(
            json_of(query("$.items[?@.price < 10].sku", DOC)),
            r#"["A","C"]"#
        );
        assert_eq!(json_of(query("$.items[1:3].sku", DOC)), r#"["B","C"]"#);
        assert_eq!(json_of(query("$.nothing[*]", DOC)), "[]");
    }

    #[test]
    fn classifies_singular_queries() {
        for singular in [
            "$",
            "$.a",
            "$.a.b",
            "$['a']",
            "$[\"a\"][0]",
            "$[-1]",
            "$.items[0].id",
            "$['a]b']",
        ] {
            assert!(is_singular(singular), "{singular}");
        }
        for plural in [
            "$.*",
            "$..a",
            "$[*]",
            "$[0,1]",
            "$[1:2]",
            "$[?@.a]",
            "$.a[*].b",
            "$['a','b']",
        ] {
            assert!(!is_singular(plural), "{plural}");
        }
    }

    #[test]
    fn rejects_invalid_queries_and_inputs() {
        assert!(JsonQuery::parse("$.[").is_err());
        let query = JsonQuery::parse("$.a").expect("valid");
        assert!(query.apply(Value::String("{".to_owned())).is_err());
        assert!(query.apply(Value::Bool(true)).is_err());
    }

    fn json_strategy() -> impl Strategy<Value = serde_json::Value> {
        let leaf = prop_oneof![
            Just(serde_json::Value::Null),
            any::<bool>().prop_map(serde_json::Value::Bool),
            any::<i64>().prop_map(serde_json::Value::from),
            (-1e9_f64..1e9).prop_map(serde_json::Value::from),
            "\\PC{0,12}".prop_map(serde_json::Value::String),
        ];
        leaf.prop_recursive(4, 32, 6, |inner| {
            prop_oneof![
                prop::collection::vec(inner.clone(), 0..6).prop_map(serde_json::Value::Array),
                prop::collection::btree_map("[a-z]{0,4}", inner, 0..6)
                    .prop_map(|map| serde_json::Value::Object(map.into_iter().collect())),
            ]
        })
    }

    proptest! {
        #[test]
        fn agrees_with_serde_json(value in json_strategy()) {
            let text = serde_json::to_string(&value).map_err(|error| TestCaseError::fail(error.to_string()))?;
            let parsed = parse(&text).map_err(|error| TestCaseError::fail(error.to_string()))?;
            prop_assert_eq!(parsed.to_json(), Some(text));
        }
    }
}
