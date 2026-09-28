//! Filters (SPEC 9.5): each takes the value before it and gives a new
//! value, a missing value, or a filter error.

use std::borrow::Cow;
use std::fmt;

use base64::engine::{DecodePaddingMode, GeneralPurpose, GeneralPurposeConfig};
use base64::{Engine as _, alphabet};
use chrono::format::{Item, StrftimeItems};
use chrono::{DateTime, NaiveDate, NaiveDateTime, Utc};

use super::json::JsonQuery;
use super::number::Number;
use super::pattern::Pattern;
use super::types::FilterKind;
use super::value::{Value, quote};
use super::xpath::{Markup, XpathQuery};

/// Base64 with the standard alphabet; decoding accepts text with or
/// without padding.
const BASE64: GeneralPurpose = GeneralPurpose::new(
    &alphabet::STANDARD,
    GeneralPurposeConfig::new().with_decode_padding_mode(DecodePaddingMode::Indifferent),
);

/// URL-safe Base64: decoding accepts padding or none; encoding writes none.
const BASE64_URL: GeneralPurpose = GeneralPurpose::new(
    &alphabet::URL_SAFE,
    GeneralPurposeConfig::new()
        .with_encode_padding(false)
        .with_decode_padding_mode(DecodePaddingMode::Indifferent),
);

/// One filter with its arguments resolved.
#[derive(Clone, Debug)]
pub(crate) enum Filter {
    Count,
    First,
    Last,
    Nth(i64),
    Split(String),
    Regex(Pattern),
    Replace {
        old: String,
        new: String,
    },
    ReplaceRegex {
        pattern:     Pattern,
        replacement: String,
    },
    ToString,
    ToInt,
    ToFloat,
    ToHex,
    ToDate(DateFormat),
    DateFormat(DateFormat),
    DaysAfterNow,
    DaysBeforeNow,
    Base64Decode,
    Base64Encode,
    Base64UrlSafeDecode,
    Base64UrlSafeEncode,
    Utf8Decode,
    Utf8Encode,
    CharsetDecode(Charset),
    UrlQueryParam(String),
    UrlEncode,
    UrlDecode,
    HtmlEscape,
    HtmlUnescape,
    Json(JsonQuery),
    Xpath(XpathQuery),
}

/// What a filter gives.
#[derive(Clone, Debug)]
pub(crate) enum Step {
    Value(Value),
    /// The filter found nothing: `urlQueryParam` with no such parameter,
    /// or a JSONPath singular query that selects nothing.
    Missing(Missing),
}

/// Why a value is missing (SPEC 9.2).
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum Missing {
    /// A locator with no match.
    NoElement,
    /// An element without the attribute. Negated predicates pass on it
    /// (SPEC 9.7).
    AbsentAttribute(String),
    /// An absent response header.
    AbsentHeader(String),
    /// A JSONPath singular query that selected nothing.
    NoJsonMatch(String),
    /// `urlQueryParam` with no such parameter.
    NoQueryParam(String),
    /// An `EXTRACT` that found no value (SPEC 7.6).
    NoExtractValue(String),
}

impl fmt::Display for Missing {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoElement => f.write_str("no matching element"),
            Self::AbsentAttribute(name) => write!(f, "attribute {name} is absent"),
            Self::AbsentHeader(name) => write!(f, "header {name} is absent"),
            Self::NoJsonMatch(path) => write!(f, "JSONPath {path} selects nothing"),
            Self::NoQueryParam(name) => write!(f, "query parameter {name} is absent"),
            Self::NoExtractValue(name) => write!(f, "EXTRACT {name} found no value"),
        }
    }
}

/// A filter that cannot work on its input (SPEC 9.5).
#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
#[error("{}: {reason}", filter.trim_end_matches(':'))]
pub(crate) struct FilterError {
    pub(crate) filter: &'static str,
    pub(crate) reason: String,
}

/// A `chrono` `%` format that parsed without errors.
#[derive(Clone, Debug)]
pub(crate) struct DateFormat(String);

impl DateFormat {
    /// Checks the format's `%` codes.
    pub(crate) fn new(format: &str) -> Result<Self, String> {
        if StrftimeItems::new(format).any(|item| matches!(item, Item::Error)) {
            return Err(format!("invalid date format {}", quote(format)));
        }
        Ok(Self(format.to_owned()))
    }
}

/// A WHATWG Encoding Standard encoding.
#[derive(Clone, Copy)]
pub(crate) struct Charset(&'static encoding_rs::Encoding);

impl Charset {
    /// Looks up an encoding label such as `gb2312` or `utf-8`.
    pub(crate) fn from_label(label: &str) -> Result<Self, String> {
        encoding_rs::Encoding::for_label(label.trim().as_bytes())
            .map(Self)
            .ok_or_else(|| format!("unknown encoding label {}", quote(label)))
    }
}

impl Charset {
    /// Decodes bytes without a byte order mark and without replacement
    /// characters.
    pub(crate) fn decode(self, bytes: &[u8]) -> Result<String, String> {
        self.0
            .decode_without_bom_handling_and_without_replacement(bytes)
            .map(std::borrow::Cow::into_owned)
            .ok_or_else(|| format!("the bytes are not valid {}", self.0.name()))
    }
}

impl Charset {
    /// Undoes a browser's decoding (SPEC 9.2). Chromium and WebKit can hand
    /// a text body back already decoded with this charset and re-encoded as
    /// UTF-8. When `bytes` are such text, gives the text and this charset's
    /// bytes for it. `None` when this charset is UTF-8 or cannot encode,
    /// when the bytes are ASCII or not UTF-8, or when the text holds a
    /// character this charset cannot encode.
    pub(crate) fn undo_browser_decode(self, bytes: &[u8]) -> Option<(String, Vec<u8>)> {
        if self.0 == encoding_rs::UTF_8 || self.0.output_encoding() != self.0 || bytes.is_ascii() {
            return None;
        }
        let text = str::from_utf8(bytes).ok()?;
        let (encoded, _, unmappable) = self.0.encode(text);
        (!unmappable).then(|| (text.to_owned(), encoded.into_owned()))
    }
}

impl fmt::Debug for Charset {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.0.name())
    }
}

impl Filter {
    /// The filter's kind, which carries its keyword and types.
    pub(crate) fn kind(&self) -> FilterKind {
        match self {
            Self::Count => FilterKind::Count,
            Self::First => FilterKind::First,
            Self::Last => FilterKind::Last,
            Self::Nth(_) => FilterKind::Nth,
            Self::Split(_) => FilterKind::Split,
            Self::Regex(_) => FilterKind::Regex,
            Self::Replace { .. } => FilterKind::Replace,
            Self::ReplaceRegex { .. } => FilterKind::ReplaceRegex,
            Self::ToString => FilterKind::ToString,
            Self::ToInt => FilterKind::ToInt,
            Self::ToFloat => FilterKind::ToFloat,
            Self::ToHex => FilterKind::ToHex,
            Self::ToDate(_) => FilterKind::ToDate,
            Self::DateFormat(_) => FilterKind::DateFormat,
            Self::DaysAfterNow => FilterKind::DaysAfterNow,
            Self::DaysBeforeNow => FilterKind::DaysBeforeNow,
            Self::Base64Decode => FilterKind::Base64Decode,
            Self::Base64Encode => FilterKind::Base64Encode,
            Self::Base64UrlSafeDecode => FilterKind::Base64UrlSafeDecode,
            Self::Base64UrlSafeEncode => FilterKind::Base64UrlSafeEncode,
            Self::Utf8Decode => FilterKind::Utf8Decode,
            Self::Utf8Encode => FilterKind::Utf8Encode,
            Self::CharsetDecode(_) => FilterKind::CharsetDecode,
            Self::UrlQueryParam(_) => FilterKind::UrlQueryParam,
            Self::UrlEncode => FilterKind::UrlEncode,
            Self::UrlDecode => FilterKind::UrlDecode,
            Self::HtmlEscape => FilterKind::HtmlEscape,
            Self::HtmlUnescape => FilterKind::HtmlUnescape,
            Self::Json(_) => FilterKind::Json,
            Self::Xpath(_) => FilterKind::Xpath,
        }
    }

    /// Applies the filter. `now` is the time the check reads its value,
    /// for `daysAfterNow` and `daysBeforeNow`; `markup` says how `xpath:`
    /// parses its input.
    pub(crate) fn apply(
        &self,
        input: Value,
        now: DateTime<Utc>,
        markup: Markup,
    ) -> Result<Step, FilterError> {
        let fail = |reason: String| FilterError {
            filter: self.kind().name(),
            reason,
        };
        let wrong_type = |actual: &Value| fail(format!("cannot take {}", actual.value_type()));
        let value = match (self, input) {
            (Self::Count, Value::List(items)) => count(items.len()),
            (Self::Count, Value::NodeSet(nodes)) => count(nodes),
            (Self::Count, Value::Bytes(bytes)) => count(bytes.len()),
            (Self::First, Value::List(items)) => items
                .into_iter()
                .next()
                .ok_or_else(|| fail("the list is empty".to_owned()))?,
            (Self::Last, Value::List(items)) => items
                .into_iter()
                .next_back()
                .ok_or_else(|| fail("the list is empty".to_owned()))?,
            (Self::Nth(index), Value::List(mut items)) => {
                let len = items.len();
                let position = resolve_index(*index, len)
                    .ok_or_else(|| fail(format!("index {index} is outside a list of {len}")))?;
                items.swap_remove(position)
            }
            (Self::Split(separator), Value::String(text)) => {
                if separator.is_empty() {
                    return Err(fail("the separator is empty".to_owned()));
                }
                Value::List(
                    text.split(separator.as_str())
                        .map(|part| Value::String(part.to_owned()))
                        .collect(),
                )
            }
            (Self::Regex(pattern), Value::String(text)) => Value::String(
                pattern
                    .extract(&text)
                    .ok_or_else(|| fail(format!("no match for {pattern:?}")))?
                    .to_owned(),
            ),
            (Self::Replace { old, new }, Value::String(text)) => {
                if old.is_empty() {
                    return Err(fail("the text to replace is empty".to_owned()));
                }
                Value::String(text.replace(old.as_str(), new))
            }
            (
                Self::ReplaceRegex {
                    pattern,
                    replacement,
                },
                Value::String(text),
            ) => Value::String(pattern.replace_all(&text, replacement)),
            (Self::ToString, value) => Value::String(
                value
                    .text_form()
                    .ok_or_else(|| fail("a node set has no text form".to_owned()))?,
            ),
            (Self::ToInt, Value::String(text)) => Value::Number(
                parse_integer(&text)
                    .ok_or_else(|| fail(format!("{} is not an integer", quote(&text))))?,
            ),
            (Self::ToInt, Value::Number(number)) => Value::Number(
                number
                    .truncate()
                    .ok_or_else(|| fail(format!("{number} is too large")))?,
            ),
            (Self::ToFloat, Value::String(text)) => Value::Number(
                text.trim()
                    .parse::<f64>()
                    .ok()
                    .and_then(Number::from_f64)
                    .ok_or_else(|| fail(format!("{} is not a number", quote(&text))))?,
            ),
            (Self::ToFloat, Value::Number(number)) => Value::Number(
                number
                    .to_float()
                    .ok_or_else(|| fail(format!("{number} is out of range")))?,
            ),
            (Self::ToHex, Value::Bytes(bytes)) => Value::String(to_hex(&bytes)),
            (Self::ToDate(format), Value::String(text)) => {
                Value::Date(parse_date(&text, &format.0).ok_or_else(|| {
                    fail(format!(
                        "{} does not match {}",
                        quote(&text),
                        quote(&format.0)
                    ))
                })?)
            }
            (Self::DateFormat(format), Value::Date(date)) => {
                Value::String(date.format(&format.0).to_string())
            }
            (Self::DaysAfterNow, Value::Date(date)) => {
                Value::Number(Number::integer((date - now).num_days()))
            }
            (Self::DaysBeforeNow, Value::Date(date)) => {
                Value::Number(Number::integer((now - date).num_days()))
            }
            (Self::Base64Decode, Value::String(text)) => Value::Bytes(
                BASE64
                    .decode(text.trim())
                    .map_err(|error| fail(error.to_string()))?,
            ),
            (Self::Base64UrlSafeDecode, Value::String(text)) => Value::Bytes(
                BASE64_URL
                    .decode(text.trim())
                    .map_err(|error| fail(error.to_string()))?,
            ),
            (Self::Base64Encode, Value::Bytes(bytes)) => Value::String(BASE64.encode(bytes)),
            (Self::Base64UrlSafeEncode, Value::Bytes(bytes)) => {
                Value::String(BASE64_URL.encode(bytes))
            }
            (Self::Utf8Decode, Value::Bytes(bytes)) => Value::String(
                String::from_utf8(bytes).map_err(|_| fail("the bytes are not UTF-8".to_owned()))?,
            ),
            (Self::Utf8Encode, Value::String(text)) => Value::Bytes(text.into_bytes()),
            (Self::CharsetDecode(charset), Value::Bytes(bytes)) => Value::String(
                charset
                    .0
                    .decode_without_bom_handling_and_without_replacement(&bytes)
                    .ok_or_else(|| fail(format!("the bytes are not valid {}", charset.0.name())))?
                    .into_owned(),
            ),
            (Self::UrlQueryParam(name), Value::String(url)) => {
                return Ok(match query_param(&url, name) {
                    Some(value) => Step::Value(Value::String(value)),
                    None => Step::Missing(Missing::NoQueryParam(name.clone())),
                });
            }
            (Self::UrlEncode, Value::String(text)) => Value::String(url_encode(&text)),
            (Self::UrlDecode, Value::String(text)) => Value::String(
                percent_encoding::percent_decode_str(&text)
                    .decode_utf8()
                    .map_err(|_| fail("the decoded bytes are not UTF-8".to_owned()))?
                    .into_owned(),
            ),
            (Self::HtmlEscape, Value::String(text)) => Value::String(escape_html(&text)),
            (Self::HtmlUnescape, Value::String(text)) => {
                Value::String(html_escape::decode_html_entities(&text).into_owned())
            }
            (Self::Json(query), value) => return query.apply(value).map_err(fail),
            (Self::Xpath(query), value) => query.apply(value, markup).map_err(fail)?,
            (_, other) => return Err(wrong_type(&other)),
        };
        Ok(Step::Value(value))
    }
}

const HEX_LOWER: &[u8; 16] = b"0123456789abcdef";
const HEX_UPPER: &[u8; 16] = b"0123456789ABCDEF";

fn count(len: usize) -> Value {
    Value::Number(Number::integer(i64::try_from(len).unwrap_or(i64::MAX)))
}

/// A 0-based index, where a negative index counts from the end.
fn resolve_index(index: i64, len: usize) -> Option<usize> {
    let len = i64::try_from(len).ok()?;
    let position = if index < 0 { len + index } else { index };
    (0..len)
        .contains(&position)
        .then(|| usize::try_from(position).ok())?
}

/// `toInt` on a string: an optional minus sign and decimal digits.
fn parse_integer(text: &str) -> Option<Number> {
    let (negative, digits) = match text.strip_prefix('-') {
        Some(digits) => (true, digits),
        None => (false, text),
    };
    if digits.is_empty() || !digits.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    let digits = digits.trim_start_matches('0');
    let normalized = match (digits.is_empty(), negative) {
        (true, _) => "0".to_owned(),
        (false, true) => format!("-{digits}"),
        (false, false) => digits.to_owned(),
    };
    Number::parse(&normalized)
}

/// `toDate`: parses with an offset when the format has one, else as UTC
/// date and time, else as a UTC date at midnight (SPEC 9.5).
fn parse_date(text: &str, format: &str) -> Option<DateTime<Utc>> {
    if let Ok(date) = DateTime::parse_from_str(text, format) {
        return Some(date.with_timezone(&Utc));
    }
    if let Ok(date) = NaiveDateTime::parse_from_str(text, format) {
        return Some(date.and_utc());
    }
    NaiveDate::parse_from_str(text, format)
        .ok()
        .and_then(|date| date.and_hms_opt(0, 0, 0))
        .map(|date| date.and_utc())
}

/// The first value of query parameter `name`, percent-decoded as form
/// data. The URL may be relative.
fn query_param(url: &str, name: &str) -> Option<String> {
    let before_fragment = url.split('#').next().unwrap_or(url);
    let (_, query) = before_fragment.split_once('?')?;
    form_urlencoded::parse(query.as_bytes())
        .find(|(key, _)| key == name)
        .map(|(_, value)| value.into_owned())
}

/// Percent-encodes every byte except unreserved characters and `/`.
/// Lowercase hexadecimal for `toHex`.
fn to_hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(char::from(HEX_LOWER[usize::from(byte >> 4)]));
        out.push(char::from(HEX_LOWER[usize::from(byte & 0xf)]));
    }
    out
}

fn url_encode(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for byte in text.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~' | b'/') {
            out.push(char::from(byte));
        } else {
            out.push('%');
            out.push(char::from(HEX_UPPER[usize::from(byte >> 4)]));
            out.push(char::from(HEX_UPPER[usize::from(byte & 0xf)]));
        }
    }
    out
}

/// Replaces `&`, `<`, and `>` with character references.
fn escape_html(text: &str) -> String {
    let escaped: Cow<'_, str> = if text.contains(['&', '<', '>']) {
        Cow::Owned(
            text.replace('&', "&amp;")
                .replace('<', "&lt;")
                .replace('>', "&gt;"),
        )
    } else {
        Cow::Borrowed(text)
    };
    escaped.into_owned()
}

#[cfg(test)]
mod tests {
    use chrono::TimeZone as _;

    use super::*;
    use crate::check::pattern::PatternFlags;

    fn now() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 9, 26, 12, 0, 0)
            .single()
            .expect("valid date")
    }

    fn apply(filter: &Filter, input: Value) -> Result<Value, FilterError> {
        match filter.apply(input, now(), Markup::Html)? {
            Step::Value(value) => Ok(value),
            Step::Missing(missing) => panic!("unexpected missing value: {missing}"),
        }
    }

    fn text(value: &str) -> Value {
        Value::String(value.to_owned())
    }

    fn text_of(result: Result<Value, FilterError>) -> String {
        result
            .expect("filter succeeds")
            .text_form()
            .expect("has a text form")
    }

    fn list(items: &[&str]) -> Value {
        Value::List(items.iter().map(|item| text(item)).collect())
    }

    #[test]
    fn counts_lists_node_sets_and_bytes() {
        assert_eq!(text_of(apply(&Filter::Count, list(&["a", "b"]))), "2");
        assert_eq!(text_of(apply(&Filter::Count, Value::NodeSet(3))), "3");
        assert_eq!(
            text_of(apply(&Filter::Count, Value::Bytes(vec![0; 5]))),
            "5"
        );
    }

    #[test]
    fn indexes_lists_from_either_end() {
        let items = list(&["a", "b", "c"]);
        assert_eq!(text_of(apply(&Filter::First, items.clone())), "a");
        assert_eq!(text_of(apply(&Filter::Last, items.clone())), "c");
        assert_eq!(text_of(apply(&Filter::Nth(1), items.clone())), "b");
        assert_eq!(text_of(apply(&Filter::Nth(-1), items.clone())), "c");
        assert!(apply(&Filter::Nth(3), items).is_err());
        assert!(apply(&Filter::First, list(&[])).is_err());
    }

    #[test]
    fn splits_strings() {
        assert_eq!(
            text_of(apply(&Filter::Split(", ".to_owned()), text("a, b,c"))),
            r#"["a","b,c"]"#
        );
        assert!(apply(&Filter::Split(String::new()), text("abc")).is_err());
    }

    #[test]
    fn extracts_and_replaces_with_regexes() {
        let pattern = Pattern::new(r"\$([\d.]+)", PatternFlags::default()).expect("valid");
        assert_eq!(
            text_of(apply(&Filter::Regex(pattern.clone()), text("now $12.50"))),
            "12.50"
        );
        assert!(apply(&Filter::Regex(pattern), text("free")).is_err());
        let digits = Pattern::new(r"[^0-9.]", PatternFlags::default()).expect("valid");
        let filter = Filter::ReplaceRegex {
            pattern:     digits,
            replacement: String::new(),
        };
        assert_eq!(text_of(apply(&filter, text("$1,299.00"))), "1299.00");
        let replace = Filter::Replace {
            old: ",".to_owned(),
            new: String::new(),
        };
        assert_eq!(text_of(apply(&replace, text("1,299"))), "1299");
    }

    #[test]
    fn converts_numbers() {
        assert_eq!(text_of(apply(&Filter::ToInt, text("007"))), "7");
        assert_eq!(text_of(apply(&Filter::ToInt, text("-12"))), "-12");
        assert!(apply(&Filter::ToInt, text("abc")).is_err());
        assert!(apply(&Filter::ToInt, text("1.5")).is_err());
        let float = Value::Number(Number::parse("-3.9").expect("valid"));
        assert_eq!(text_of(apply(&Filter::ToInt, float)), "-3");
        assert_eq!(text_of(apply(&Filter::ToFloat, text("3"))), "3.0");
        assert_eq!(text_of(apply(&Filter::ToFloat, text("3.14"))), "3.14");
        assert!(apply(&Filter::ToFloat, text("inf")).is_err());
        assert_eq!(text_of(apply(&Filter::ToString, Value::Bool(true))), "true");
    }

    #[test]
    fn converts_bytes() {
        let bytes = Value::Bytes(b"<<???>>".to_vec());
        assert_eq!(
            text_of(apply(&Filter::Base64Encode, bytes.clone())),
            "PDw/Pz8+Pg=="
        );
        assert_eq!(
            text_of(apply(&Filter::Base64UrlSafeEncode, bytes.clone())),
            "PDw_Pz8-Pg"
        );
        assert_eq!(text_of(apply(&Filter::ToHex, bytes)), "3c3c3f3f3f3e3e");
        let decoded = apply(&Filter::Base64UrlSafeDecode, text("PDw_Pz8-Pg")).expect("decodes");
        assert!(matches!(decoded, Value::Bytes(bytes) if bytes == b"<<???>>"));
        assert!(apply(&Filter::Base64Decode, text("!!")).is_err());
        let cafe = apply(&Filter::Utf8Encode, text("café")).expect("encodes");
        assert_eq!(text_of(apply(&Filter::ToHex, cafe.clone())), "636166c3a9");
        assert_eq!(text_of(apply(&Filter::Utf8Decode, cafe)), "café");
        assert!(apply(&Filter::Utf8Decode, Value::Bytes(vec![0xff])).is_err());
        let gb2312 = Charset::from_label("gb2312").expect("known label");
        let hello = Value::Bytes(vec![0xc4, 0xe3, 0xba, 0xc3]);
        assert_eq!(
            text_of(apply(&Filter::CharsetDecode(gb2312), hello)),
            "你好"
        );
        assert!(Charset::from_label("nope").is_err());
    }

    #[test]
    fn undoes_a_browsers_decoding_only_when_it_round_trips() {
        let latin = Charset::from_label("iso-8859-1").expect("known label");
        // "café\u{80}" decoded as windows-1252 and re-encoded as UTF-8.
        let decoded = "caf\u{e9}\u{20ac}".as_bytes();
        assert_eq!(
            latin.undo_browser_decode(decoded),
            Some(("caf\u{e9}\u{20ac}".to_owned(), vec![
                0x63, 0x61, 0x66, 0xe9, 0x80
            ]))
        );
        // Raw bytes that are not UTF-8, ASCII text, and U+FFFD stay as they are.
        assert_eq!(latin.undo_browser_decode(&[0x63, 0xe9]), None);
        assert_eq!(latin.undo_browser_decode(b"cafe"), None);
        assert_eq!(latin.undo_browser_decode("caf\u{fffd}".as_bytes()), None);
        let utf8 = Charset::from_label("utf-8").expect("known label");
        assert_eq!(utf8.undo_browser_decode("caf\u{e9}".as_bytes()), None);
    }

    #[test]
    fn reads_urls() {
        let url = text("https://example.com/search?q=caf%C3%A9+au+lait&page=2&page=3#top");
        assert_eq!(
            text_of(apply(
                &Filter::UrlQueryParam("page".to_owned()),
                url.clone()
            )),
            "2"
        );
        assert_eq!(
            text_of(apply(&Filter::UrlQueryParam("q".to_owned()), url.clone())),
            "café au lait"
        );
        let missing = Filter::UrlQueryParam("debug".to_owned()).apply(url, now(), Markup::Html);
        assert!(matches!(
            missing,
            Ok(Step::Missing(Missing::NoQueryParam(_)))
        ));
        let relative = text("/cart?step=2");
        assert_eq!(
            text_of(apply(&Filter::UrlQueryParam("step".to_owned()), relative)),
            "2"
        );
        assert_eq!(
            text_of(apply(&Filter::UrlEncode, text("/cart?step=2"))),
            "/cart%3Fstep%3D2"
        );
        assert_eq!(
            text_of(apply(
                &Filter::UrlEncode,
                text("https://mozilla.org/?x=шеллы")
            )),
            "https%3A//mozilla.org/%3Fx%3D%D1%88%D0%B5%D0%BB%D0%BB%D1%8B"
        );
        assert_eq!(
            text_of(apply(&Filter::UrlDecode, text("/search?q=caf%C3%A9"))),
            "/search?q=café"
        );
    }

    #[test]
    fn escapes_html() {
        assert_eq!(
            text_of(apply(&Filter::HtmlEscape, text("a > b & c"))),
            "a &gt; b &amp; c"
        );
        assert_eq!(
            text_of(apply(
                &Filter::HtmlUnescape,
                text("Foo &copy; bar &#x1D306; &#62; &gt;")
            )),
            "Foo © bar 𝌆 > >"
        );
    }

    #[test]
    fn parses_formats_and_counts_dates() {
        let iso = DateFormat::new("%+").expect("valid");
        let date = apply(&Filter::ToDate(iso), text("2026-10-26T12:00:00Z")).expect("parses");
        assert_eq!(text_of(apply(&Filter::DaysAfterNow, date.clone())), "30");
        assert_eq!(text_of(apply(&Filter::DaysBeforeNow, date.clone())), "-30");
        let year = DateFormat::new("%Y").expect("valid");
        assert_eq!(text_of(apply(&Filter::DateFormat(year), date)), "2026");
        let http = DateFormat::new("%a, %d %b %Y %H:%M:%S GMT").expect("valid");
        let modified =
            apply(&Filter::ToDate(http), text("Wed, 23 Sep 2026 12:00:00 GMT")).expect("parses");
        assert_eq!(text_of(apply(&Filter::DaysBeforeNow, modified)), "3");
        let day = DateFormat::new("%Y-%m-%d").expect("valid");
        assert_eq!(
            text_of(apply(&Filter::ToDate(day), text("2026-09-01"))),
            "2026-09-01T00:00:00Z"
        );
        assert!(DateFormat::new("%Q").is_err());
    }

    #[test]
    fn rejects_the_wrong_input_type() {
        let error = apply(&Filter::ToHex, text("abc")).expect_err("toHex needs bytes");
        assert_eq!(error.filter, "toHex");
        assert_eq!(error.reason, "cannot take string");
    }
}
