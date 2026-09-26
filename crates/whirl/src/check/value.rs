//! Typed check values (SPEC 9.3): the types, their text forms, and JSON
//! equality (SPEC 9.6).

use std::fmt;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use chrono::{DateTime, SecondsFormat, Utc};

use super::number::Number;

/// A value that a subject or a filter gives, or that a capture stores.
#[derive(Clone, Debug)]
pub(crate) enum Value {
    String(String),
    Number(Number),
    Bool(bool),
    Null,
    List(Vec<Self>),
    /// Members in the order received. Keys are unique: a JSON object that
    /// repeats a key keeps the last value at the first key's position.
    Object(Vec<(String, Self)>),
    Bytes(Vec<u8>),
    Date(DateTime<Utc>),
    /// An XPath node set, which supports only `count` and `exists`.
    #[cfg_attr(
        not(test),
        expect(
            dead_code,
            reason = "the xpath: filter arrives with libxml2 in milestone M7"
        )
    )]
    NodeSet(usize),
}

/// The type of a [`Value`], named as SPEC 9.3 names it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ValueType {
    String,
    Number,
    Boolean,
    Null,
    List,
    Object,
    Bytes,
    Date,
    NodeSet,
}

impl ValueType {
    /// The SPEC name, such as `string` or `node set`.
    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::String => "string",
            Self::Number => "number",
            Self::Boolean => "boolean",
            Self::Null => "null",
            Self::List => "list",
            Self::Object => "object",
            Self::Bytes => "bytes",
            Self::Date => "date",
            Self::NodeSet => "node set",
        }
    }
}

impl fmt::Display for ValueType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

impl Value {
    /// A value from the shim's JSON transport: a page string, a count, or
    /// an `eval` result (SPEC 10).
    pub(crate) fn from_json(json: serde_json::Value) -> Self {
        match json {
            serde_json::Value::Null => Self::Null,
            serde_json::Value::Bool(value) => Self::Bool(value),
            serde_json::Value::Number(number) => {
                Number::parse(&number.to_string()).map_or(Self::Null, Self::Number)
            }
            serde_json::Value::String(text) => Self::String(text),
            serde_json::Value::Array(items) => {
                Self::List(items.into_iter().map(Self::from_json).collect())
            }
            serde_json::Value::Object(members) => Self::Object(
                members
                    .into_iter()
                    .map(|(key, value)| (key, Self::from_json(value)))
                    .collect(),
            ),
        }
    }

    /// The value's type.
    pub(crate) fn value_type(&self) -> ValueType {
        match self {
            Self::String(_) => ValueType::String,
            Self::Number(_) => ValueType::Number,
            Self::Bool(_) => ValueType::Boolean,
            Self::Null => ValueType::Null,
            Self::List(_) => ValueType::List,
            Self::Object(_) => ValueType::Object,
            Self::Bytes(_) => ValueType::Bytes,
            Self::Date(_) => ValueType::Date,
            Self::NodeSet(_) => ValueType::NodeSet,
        }
    }

    /// The text form (SPEC 9.3), used for interpolation, reports, and
    /// `toString`. A node set has none.
    pub(crate) fn text_form(&self) -> Option<String> {
        Some(match self {
            Self::String(text) => text.clone(),
            Self::Number(number) => number.text().to_owned(),
            Self::Bool(value) => value.to_string(),
            Self::Null => "null".to_owned(),
            Self::List(_) | Self::Object(_) => self.to_json()?,
            Self::Bytes(bytes) => STANDARD.encode(bytes),
            Self::Date(date) => date.to_rfc3339_opts(SecondsFormat::AutoSi, true),
            Self::NodeSet(_) => return None,
        })
    }

    /// Compact JSON for the value. Bytes and dates become JSON strings of
    /// their text form. A node set has none.
    pub(crate) fn to_json(&self) -> Option<String> {
        let mut out = String::new();
        self.write_json(&mut out)?;
        Some(out)
    }

    fn write_json(&self, out: &mut String) -> Option<()> {
        match self {
            Self::String(text) => push_json_string(out, text),
            Self::Number(number) => out.push_str(number.text()),
            Self::Bool(value) => out.push_str(if *value { "true" } else { "false" }),
            Self::Null => out.push_str("null"),
            Self::List(items) => {
                out.push('[');
                for (index, item) in items.iter().enumerate() {
                    if index > 0 {
                        out.push(',');
                    }
                    item.write_json(out)?;
                }
                out.push(']');
            }
            Self::Object(members) => {
                out.push('{');
                for (index, (key, item)) in members.iter().enumerate() {
                    if index > 0 {
                        out.push(',');
                    }
                    push_json_string(out, key);
                    out.push(':');
                    item.write_json(out)?;
                }
                out.push('}');
            }
            Self::Bytes(_) | Self::Date(_) => push_json_string(out, &self.text_form()?),
            Self::NodeSet(_) => return None,
        }
        Some(())
    }

    /// JSON equality (SPEC 9.6). Values of different types are never
    /// equal; integers and floats are both numbers.
    pub(crate) fn json_eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::String(a), Self::String(b)) => a == b,
            (Self::Number(a), Self::Number(b)) => a == b,
            (Self::Bool(a), Self::Bool(b)) => a == b,
            (Self::Null, Self::Null) => true,
            (Self::List(a), Self::List(b)) => {
                a.len() == b.len() && a.iter().zip(b).all(|(x, y)| x.json_eq(y))
            }
            (Self::Object(a), Self::Object(b)) => {
                a.len() == b.len()
                    && a.iter().all(|(key, x)| {
                        b.iter()
                            .find(|(other_key, _)| other_key == key)
                            .is_some_and(|(_, y)| x.json_eq(y))
                    })
            }
            (Self::Bytes(a), Self::Bytes(b)) => a == b,
            (Self::Date(a), Self::Date(b)) => a == b,
            (Self::NodeSet(a), Self::NodeSet(b)) => a == b,
            _ => false,
        }
    }

    /// The value as a failure report shows it: a string in JSON quotes,
    /// so `"42"` and `42` read differently, and other values in their
    /// text form, with the type named where the text form is ambiguous.
    pub(crate) fn describe(&self) -> String {
        match self {
            Self::String(text) => quote(text),
            Self::Bytes(_) | Self::Date(_) => format!(
                "{} {}",
                self.value_type(),
                self.text_form().unwrap_or_default()
            ),
            Self::NodeSet(count) => format!("node set of {count}"),
            other => other.text_form().unwrap_or_default(),
        }
    }
}

/// Writes `text` as a JSON string literal.
fn push_json_string(out: &mut String, text: &str) {
    out.push('"');
    for ch in text.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            ch if u32::from(ch) < 0x20 => {
                let code = u32::from(ch);
                out.push_str("\\u00");
                out.extend([hex_digit(code >> 4), hex_digit(code & 0xf)]);
            }
            ch => out.push(ch),
        }
    }
    out.push('"');
}

fn hex_digit(nibble: u32) -> char {
    char::from_digit(nibble, 16).unwrap_or('0')
}

/// `text` as a JSON string literal, for reports.
pub(crate) fn quote(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 2);
    push_json_string(&mut out, text);
    out
}

#[cfg(test)]
mod tests {
    use chrono::TimeZone as _;

    use super::*;

    fn num(text: &str) -> Value {
        Value::Number(Number::parse(text).expect("valid number"))
    }

    fn string(text: &str) -> Value {
        Value::String(text.to_owned())
    }

    #[test]
    fn text_forms_follow_the_spec_table() {
        let object = Value::Object(vec![
            ("b".to_owned(), num("1.50")),
            (
                "a".to_owned(),
                Value::List(vec![string("x\"y"), Value::Null]),
            ),
        ]);
        assert_eq!(
            object.text_form().as_deref(),
            Some(r#"{"b":1.50,"a":["x\"y",null]}"#)
        );
        assert_eq!(
            Value::Bytes(b"<<???>>".to_vec()).text_form().as_deref(),
            Some("PDw/Pz8+Pg==")
        );
        let date = Utc
            .with_ymd_and_hms(2026, 9, 26, 8, 0, 0)
            .single()
            .expect("valid date");
        assert_eq!(
            Value::Date(date).text_form().as_deref(),
            Some("2026-09-26T08:00:00Z")
        );
        assert_eq!(Value::NodeSet(2).text_form(), None);
    }

    #[test]
    fn json_equality_ignores_key_order_and_number_form() {
        let a = Value::Object(vec![
            ("w".to_owned(), num("10")),
            ("h".to_owned(), num("20")),
        ]);
        let b = Value::Object(vec![
            ("h".to_owned(), num("2e1")),
            ("w".to_owned(), num("10.0")),
        ]);
        assert!(a.json_eq(&b));
    }

    #[test]
    fn json_equality_keeps_list_order_and_types() {
        assert!(
            !Value::List(vec![num("1"), num("2")]).json_eq(&Value::List(vec![num("2"), num("1")]))
        );
        assert!(!num("42").json_eq(&string("42")));
        assert!(!Value::Null.json_eq(&Value::Bool(false)));
    }

    #[test]
    fn describes_values_with_their_type() {
        assert_eq!(string("42").describe(), "\"42\"");
        assert_eq!(num("42").describe(), "42");
        assert_eq!(Value::List(vec![]).describe(), "[]");
        assert_eq!(Value::Bytes(vec![1]).describe(), "bytes AQ==");
    }
}
