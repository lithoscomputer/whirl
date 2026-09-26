//! Predicates (SPEC 9.4) and the expected value's typed reading (SPEC
//! 9.6).

use std::net::{Ipv4Addr, Ipv6Addr};

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use chrono::DateTime;

use super::number::Number;
use super::pattern::Pattern;
use super::types::PredicateKind;
use super::value::{Value, ValueType, quote};

/// A predicate with its expected value resolved.
#[derive(Clone, Debug)]
pub(crate) enum Predicate {
    Compare(PredicateKind, Expected),
    Matches(Pattern),
    /// `exists` and the `is…` type predicates.
    Word(PredicateKind),
}

/// The expected value after SPEC 9.6's reading.
#[derive(Clone, Debug)]
pub(crate) enum Expected {
    /// The value under test is always a string, so the expected value is
    /// its text.
    Text(String),
    /// A typed value.
    Typed(Value),
    /// A bare `hex,…;` or `base64,…;` literal: bytes when the value under
    /// test is bytes, and otherwise the string it was written as.
    BytesLiteral { bytes: Vec<u8>, text: String },
}

impl Expected {
    /// Reads a bare expected value in a typed comparison: a JSON number,
    /// `true`, `false`, `null`, or a bytes literal, else a string.
    pub(crate) fn bare(text: String) -> Self {
        if let Some(number) = Number::parse(&text) {
            return Self::Typed(Value::Number(number));
        }
        match text.as_str() {
            "true" => Self::Typed(Value::Bool(true)),
            "false" => Self::Typed(Value::Bool(false)),
            "null" => Self::Typed(Value::Null),
            _ => match bytes_literal(&text) {
                Some(bytes) => Self::BytesLiteral { bytes, text },
                None => Self::Typed(Value::String(text)),
            },
        }
    }

    /// The expected value as written in a failure report.
    fn describe(&self) -> String {
        match self {
            Self::Text(text) | Self::Typed(Value::String(text)) => quote(text),
            Self::Typed(value) => value.text_form().unwrap_or_default(),
            Self::BytesLiteral { text, .. } => text.clone(),
        }
    }

    /// The expected value as a string, when it can be one.
    fn as_text(&self) -> Option<&str> {
        match self {
            Self::Text(text)
            | Self::Typed(Value::String(text))
            | Self::BytesLiteral { text, .. } => Some(text),
            Self::Typed(_) => None,
        }
    }

    /// The expected value as bytes, when it can be bytes.
    fn as_bytes(&self) -> Option<&[u8]> {
        match self {
            Self::BytesLiteral { bytes, .. } | Self::Typed(Value::Bytes(bytes)) => Some(bytes),
            _ => None,
        }
    }

    /// The expected value as a typed value, for JSON equality.
    fn to_value(&self) -> Value {
        match self {
            Self::Text(text) | Self::BytesLiteral { text, .. } => Value::String(text.clone()),
            Self::Typed(value) => value.clone(),
        }
    }
}

/// Decodes a bytes literal: `hex,DIGITS;` or `base64,TEXT;`.
pub(crate) fn bytes_literal(text: &str) -> Option<Vec<u8>> {
    let body = text.strip_suffix(';')?;
    if let Some(hex) = body.strip_prefix("hex,") {
        if hex.len() % 2 != 0 {
            return None;
        }
        return (0..hex.len())
            .step_by(2)
            .map(|at| u8::from_str_radix(hex.get(at..at + 2)?, 16).ok())
            .collect();
    }
    body.strip_prefix("base64,")
        .and_then(|encoded| STANDARD.decode(encoded).ok())
}

/// The outcome of testing one value.
pub(crate) enum Outcome {
    /// The predicate holds or does not.
    Decided {
        holds:        bool,
        types_differ: bool,
    },
    /// The predicate cannot test a value of this type.
    TypeMismatch,
}

impl Predicate {
    /// The predicate's kind.
    pub(crate) fn kind(&self) -> PredicateKind {
        match self {
            Self::Compare(kind, _) | Self::Word(kind) => *kind,
            Self::Matches(_) => PredicateKind::Matches,
        }
    }

    /// The predicate as written, such as `== 42` or `isInteger`.
    pub(crate) fn describe(&self) -> String {
        match self {
            Self::Compare(kind, expected) => format!("{} {}", kind.name(), expected.describe()),
            Self::Matches(pattern) => format!("matches {pattern:?}"),
            Self::Word(kind) => kind.name().to_owned(),
        }
    }

    /// Tests a present value.
    pub(crate) fn test(&self, value: &Value) -> Outcome {
        let decided = |holds: bool| Outcome::Decided {
            holds,
            types_differ: false,
        };
        match self {
            // A node set supports only `count` and `exists` (SPEC 9.3).
            Self::Compare(..) if matches!(value, Value::NodeSet(_)) => Outcome::TypeMismatch,
            Self::Compare(kind, expected) => compare(*kind, value, expected),
            Self::Matches(pattern) => match value {
                Value::String(text) => decided(pattern.is_match(text)),
                _ => Outcome::TypeMismatch,
            },
            Self::Word(kind) => word(*kind, value),
        }
    }
}

fn compare(kind: PredicateKind, value: &Value, expected: &Expected) -> Outcome {
    let decided = |holds: bool| Outcome::Decided {
        holds,
        types_differ: false,
    };
    match kind {
        PredicateKind::Eq | PredicateKind::Ne => {
            let (equal, types_differ) = equals(value, expected);
            Outcome::Decided {
                holds: if kind == PredicateKind::Eq {
                    equal
                } else {
                    !equal
                },
                types_differ,
            }
        }
        PredicateKind::Gt | PredicateKind::Ge | PredicateKind::Lt | PredicateKind::Le => {
            let ordering = match (value, expected) {
                (Value::Number(a), Expected::Typed(Value::Number(b))) => a.cmp_value(b),
                (Value::Date(a), Expected::Typed(Value::Date(b))) => a.cmp(b),
                _ => return Outcome::TypeMismatch,
            };
            decided(match kind {
                PredicateKind::Gt => ordering.is_gt(),
                PredicateKind::Ge => ordering.is_ge(),
                PredicateKind::Lt => ordering.is_lt(),
                _ => ordering.is_le(),
            })
        }
        PredicateKind::StartsWith | PredicateKind::EndsWith => {
            let starts = kind == PredicateKind::StartsWith;
            match value {
                Value::String(text) => match expected.as_text() {
                    Some(prefix) if starts => decided(text.starts_with(prefix)),
                    Some(suffix) => decided(text.ends_with(suffix)),
                    None => Outcome::TypeMismatch,
                },
                Value::Bytes(bytes) => match expected.as_bytes() {
                    Some(prefix) if starts => decided(bytes.starts_with(prefix)),
                    Some(suffix) => decided(bytes.ends_with(suffix)),
                    None => Outcome::TypeMismatch,
                },
                _ => Outcome::TypeMismatch,
            }
        }
        PredicateKind::Contains => match value {
            Value::String(text) => expected.as_text().map_or(Outcome::TypeMismatch, |needle| {
                decided(text.contains(needle))
            }),
            Value::List(items) => {
                let needle = expected.to_value();
                decided(items.iter().any(|item| item.json_eq(&needle)))
            }
            Value::Bytes(bytes) => expected.as_bytes().map_or(Outcome::TypeMismatch, |needle| {
                decided(needle.is_empty() || bytes.windows(needle.len()).any(|w| w == needle))
            }),
            _ => Outcome::TypeMismatch,
        },
        _ => Outcome::TypeMismatch,
    }
}

/// JSON equality between a value and the expected value, and whether
/// their types differ.
fn equals(value: &Value, expected: &Expected) -> (bool, bool) {
    match (value, expected) {
        (_, Expected::Text(text)) => (value.text_form().as_deref() == Some(text), false),
        (Value::Bytes(bytes), Expected::BytesLiteral { bytes: literal, .. }) => {
            (bytes == literal, false)
        }
        _ => {
            let expected = expected.to_value();
            let equal = value.json_eq(&expected);
            (equal, value.value_type() != expected.value_type())
        }
    }
}

fn word(kind: PredicateKind, value: &Value) -> Outcome {
    let holds = match kind {
        PredicateKind::Exists => !matches!(value, Value::NodeSet(0)),
        PredicateKind::IsBoolean => matches!(value, Value::Bool(_)),
        PredicateKind::IsEmpty => match value {
            Value::List(items) => items.is_empty(),
            Value::Object(members) => members.is_empty(),
            _ => return Outcome::TypeMismatch,
        },
        PredicateKind::IsFloat => matches!(value, Value::Number(number) if number.is_float()),
        PredicateKind::IsInteger => matches!(value, Value::Number(number) if !number.is_float()),
        PredicateKind::IsIpv4 => string(value).is_some_and(|text| text.parse::<Ipv4Addr>().is_ok()),
        PredicateKind::IsIpv6 => string(value).is_some_and(|text| text.parse::<Ipv6Addr>().is_ok()),
        PredicateKind::IsIsoDate => {
            string(value).is_some_and(|text| DateTime::parse_from_rfc3339(text).is_ok())
        }
        PredicateKind::IsList => value.value_type() == ValueType::List,
        PredicateKind::IsNumber => value.value_type() == ValueType::Number,
        PredicateKind::IsObject => value.value_type() == ValueType::Object,
        PredicateKind::IsString => value.value_type() == ValueType::String,
        PredicateKind::IsUuid => string(value).is_some_and(is_uuid_v4),
        _ => return Outcome::TypeMismatch,
    };
    Outcome::Decided {
        holds,
        types_differ: false,
    }
}

fn string(value: &Value) -> Option<&str> {
    match value {
        Value::String(text) => Some(text),
        _ => None,
    }
}

/// A version 4 UUID: `xxxxxxxx-xxxx-4xxx-Nxxx-xxxxxxxxxxxx`, where `N` is
/// `8`, `9`, `a`, or `b`, in either case.
fn is_uuid_v4(text: &str) -> bool {
    let groups: Vec<&str> = text.split('-').collect();
    let lengths_ok = groups.iter().map(|group| group.len()).eq([8, 4, 4, 4, 12]);
    let hex_ok = groups
        .iter()
        .all(|group| group.bytes().all(|byte| byte.is_ascii_hexdigit()));
    lengths_ok
        && hex_ok
        && groups[2].starts_with('4')
        && groups[3].starts_with(['8', '9', 'a', 'b', 'A', 'B'])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::check::pattern::PatternFlags;

    fn num(text: &str) -> Value {
        Value::Number(Number::parse(text).expect("valid"))
    }

    fn holds(predicate: &Predicate, value: &Value) -> Option<bool> {
        match predicate.test(value) {
            Outcome::Decided { holds, .. } => Some(holds),
            Outcome::TypeMismatch => None,
        }
    }

    fn compare(kind: PredicateKind, expected: Expected) -> Predicate {
        Predicate::Compare(kind, expected)
    }

    #[test]
    fn reads_bare_values_as_typed_literals() {
        assert!(matches!(
            Expected::bare("42".to_owned()),
            Expected::Typed(Value::Number(_))
        ));
        assert!(matches!(
            Expected::bare("true".to_owned()),
            Expected::Typed(Value::Bool(true))
        ));
        assert!(matches!(
            Expected::bare("null".to_owned()),
            Expected::Typed(Value::Null)
        ));
        assert!(matches!(
            Expected::bare("007".to_owned()),
            Expected::Typed(Value::String(_))
        ));
        assert!(matches!(
            Expected::bare("hex,89504e47;".to_owned()),
            Expected::BytesLiteral { .. }
        ));
        assert!(matches!(
            Expected::bare("paid".to_owned()),
            Expected::Typed(Value::String(_))
        ));
    }

    #[test]
    fn decodes_bytes_literals() {
        assert_eq!(bytes_literal("hex,beef;"), Some(vec![0xbe, 0xef]));
        assert_eq!(
            bytes_literal("base64,PDw/Pz8+Pg==;"),
            Some(b"<<???>>".to_vec())
        );
        assert_eq!(bytes_literal("hex,abc;"), None);
        assert_eq!(bytes_literal("hex,beef"), None);
    }

    #[test]
    fn equality_is_typed() {
        let eq = compare(PredicateKind::Eq, Expected::bare("42".to_owned()));
        assert_eq!(holds(&eq, &num("42")), Some(true));
        assert_eq!(holds(&eq, &num("42.0")), Some(true));
        assert_eq!(holds(&eq, &Value::String("42".to_owned())), Some(false));
        let Outcome::Decided { types_differ, .. } = eq.test(&Value::String("42".to_owned())) else {
            panic!("equality decides");
        };
        assert!(types_differ);
        let ne = compare(PredicateKind::Ne, Expected::bare("42".to_owned()));
        assert_eq!(holds(&ne, &Value::String("42".to_owned())), Some(true));
    }

    #[test]
    fn text_equality_ignores_quotes() {
        let eq = compare(PredicateKind::Eq, Expected::Text("1".to_owned()));
        assert_eq!(holds(&eq, &Value::String("1".to_owned())), Some(true));
    }

    #[test]
    fn orders_numbers_and_rejects_other_types() {
        let gt = compare(PredicateKind::Gt, Expected::bare("10".to_owned()));
        assert_eq!(holds(&gt, &num("10.5")), Some(true));
        assert_eq!(holds(&gt, &num("1e1")), Some(false));
        assert_eq!(holds(&gt, &Value::String("11".to_owned())), None);
    }

    #[test]
    fn contains_depends_on_the_type() {
        let admin = compare(PredicateKind::Contains, Expected::bare("admin".to_owned()));
        let roles = Value::List(vec![Value::String("superadmin".to_owned())]);
        assert_eq!(holds(&admin, &roles), Some(false));
        assert_eq!(
            holds(&admin, &Value::String("superadmin".to_owned())),
            Some(true)
        );
        assert_eq!(holds(&admin, &Value::Object(vec![])), None);
        let beef = compare(
            PredicateKind::Contains,
            Expected::bare("hex,beef;".to_owned()),
        );
        assert_eq!(
            holds(&beef, &Value::Bytes(vec![1, 0xbe, 0xef, 2])),
            Some(true)
        );
        let number = compare(PredicateKind::Contains, Expected::bare("42".to_owned()));
        assert_eq!(holds(&number, &Value::List(vec![num("42")])), Some(true));
        assert_eq!(
            holds(&number, &Value::List(vec![Value::String("42".to_owned())])),
            Some(false)
        );
    }

    #[test]
    fn starts_and_ends_with_strings_and_bytes() {
        let png = compare(
            PredicateKind::StartsWith,
            Expected::bare("hex,89504e47;".to_owned()),
        );
        assert_eq!(
            holds(&png, &Value::Bytes(vec![0x89, 0x50, 0x4e, 0x47, 0])),
            Some(true)
        );
        let pdf = compare(PredicateKind::EndsWith, Expected::Text(".pdf".to_owned()));
        assert_eq!(
            holds(&pdf, &Value::String("/a/b.pdf".to_owned())),
            Some(true)
        );
    }

    #[test]
    fn matches_strings_only() {
        let pattern = Pattern::new(r"^Order \d+$", PatternFlags::default()).expect("valid");
        let matches = Predicate::Matches(pattern);
        assert_eq!(
            holds(&matches, &Value::String("Order 42".to_owned())),
            Some(true)
        );
        assert_eq!(holds(&matches, &num("42")), None);
    }

    #[test]
    fn checks_types_and_formats() {
        let word = |kind| Predicate::Word(kind);
        assert_eq!(
            holds(&word(PredicateKind::IsInteger), &num("3")),
            Some(true)
        );
        assert_eq!(
            holds(&word(PredicateKind::IsFloat), &num("3.0")),
            Some(true)
        );
        assert_eq!(
            holds(
                &word(PredicateKind::IsNumber),
                &Value::String("3".to_owned())
            ),
            Some(false)
        );
        assert_eq!(
            holds(&word(PredicateKind::IsEmpty), &Value::List(vec![])),
            Some(true)
        );
        assert_eq!(
            holds(&word(PredicateKind::IsEmpty), &Value::String(String::new())),
            None
        );
        let text = |t: &str| Value::String(t.to_owned());
        assert_eq!(
            holds(&word(PredicateKind::IsIpv4), &text("192.168.0.1")),
            Some(true)
        );
        assert_eq!(
            holds(&word(PredicateKind::IsIpv6), &text("::1")),
            Some(true)
        );
        assert_eq!(
            holds(
                &word(PredicateKind::IsIsoDate),
                &text("2026-09-26T08:00:00.000Z")
            ),
            Some(true)
        );
        assert_eq!(
            holds(&word(PredicateKind::IsIsoDate), &text("2026-09-26")),
            Some(false)
        );
        let uuid = "f47ac10b-58cc-4372-a567-0e02b2c3d479";
        assert_eq!(holds(&word(PredicateKind::IsUuid), &text(uuid)), Some(true));
        let v1 = "f47ac10b-58cc-1372-a567-0e02b2c3d479";
        assert_eq!(holds(&word(PredicateKind::IsUuid), &text(v1)), Some(false));
    }
}
