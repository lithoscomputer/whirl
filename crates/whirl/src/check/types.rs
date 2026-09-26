//! Static types for checks (SPEC 9.4-9.6): what `whirl check` can know
//! about a value before the run. The lint `filter-type` reports a chain
//! whose types cannot work, and SPEC 9.6 reads the expected value as text
//! only when the static type is a string.

use std::fmt;

use super::value::ValueType;

/// A value's type as far as the parser can see.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum StaticType {
    Known(ValueType),
    /// A list whose items have this type.
    ListOf(Box<Self>),
    /// JSON values and `eval` results: known only at run time.
    Any,
}

impl StaticType {
    pub(crate) const STRING: Self = Self::Known(ValueType::String);
    pub(crate) const NUMBER: Self = Self::Known(ValueType::Number);
    pub(crate) const BYTES: Self = Self::Known(ValueType::Bytes);

    /// The concrete type, when known.
    fn value_type(&self) -> Option<ValueType> {
        match self {
            Self::Known(value_type) => Some(*value_type),
            Self::ListOf(_) => Some(ValueType::List),
            Self::Any => None,
        }
    }

    /// True when the value is always a string, so the expected value is
    /// text (SPEC 9.6).
    pub(crate) fn is_string(&self) -> bool {
        *self == Self::STRING
    }

    fn accepted_by(&self, accepted: &[ValueType]) -> bool {
        self.value_type()
            .is_none_or(|value_type| accepted.contains(&value_type))
    }
}

impl fmt::Display for StaticType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Known(value_type) => value_type.fmt(f),
            Self::ListOf(_) => f.write_str("list"),
            Self::Any => f.write_str("any value"),
        }
    }
}

/// A filter without its arguments (SPEC 9.5).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum FilterKind {
    Count,
    First,
    Last,
    Nth,
    Split,
    Regex,
    Replace,
    ReplaceRegex,
    ToString,
    ToInt,
    ToFloat,
    ToHex,
    ToDate,
    DateFormat,
    DaysAfterNow,
    DaysBeforeNow,
    Base64Decode,
    Base64Encode,
    Base64UrlSafeDecode,
    Base64UrlSafeEncode,
    Utf8Decode,
    Utf8Encode,
    CharsetDecode,
    UrlQueryParam,
    UrlEncode,
    UrlDecode,
    HtmlEscape,
    HtmlUnescape,
    Json,
    Xpath,
}

/// Every filter keyword that has no `:` argument, with its kind.
pub(crate) const FILTER_KEYWORDS: &[(&str, FilterKind)] = &[
    ("count", FilterKind::Count),
    ("first", FilterKind::First),
    ("last", FilterKind::Last),
    ("nth", FilterKind::Nth),
    ("split", FilterKind::Split),
    ("regex", FilterKind::Regex),
    ("replace", FilterKind::Replace),
    ("replaceRegex", FilterKind::ReplaceRegex),
    ("toString", FilterKind::ToString),
    ("toInt", FilterKind::ToInt),
    ("toFloat", FilterKind::ToFloat),
    ("toHex", FilterKind::ToHex),
    ("toDate", FilterKind::ToDate),
    ("dateFormat", FilterKind::DateFormat),
    ("daysAfterNow", FilterKind::DaysAfterNow),
    ("daysBeforeNow", FilterKind::DaysBeforeNow),
    ("base64Decode", FilterKind::Base64Decode),
    ("base64Encode", FilterKind::Base64Encode),
    ("base64UrlSafeDecode", FilterKind::Base64UrlSafeDecode),
    ("base64UrlSafeEncode", FilterKind::Base64UrlSafeEncode),
    ("utf8Decode", FilterKind::Utf8Decode),
    ("utf8Encode", FilterKind::Utf8Encode),
    ("charsetDecode", FilterKind::CharsetDecode),
    ("urlQueryParam", FilterKind::UrlQueryParam),
    ("urlEncode", FilterKind::UrlEncode),
    ("urlDecode", FilterKind::UrlDecode),
    ("htmlEscape", FilterKind::HtmlEscape),
    ("htmlUnescape", FilterKind::HtmlUnescape),
];

impl FilterKind {
    /// The filter's keyword in `.whirl` source.
    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::Json => "json:",
            Self::Xpath => "xpath:",
            kind => FILTER_KEYWORDS
                .iter()
                .find(|(_, candidate)| *candidate == kind)
                .map_or("?", |(name, _)| name),
        }
    }

    /// The input types the filter accepts (SPEC 9.5).
    pub(crate) fn input_types(self) -> &'static [ValueType] {
        use ValueType::{Boolean, Bytes, Date, List, NodeSet, Null, Number, Object, String};
        match self {
            Self::Count => &[List, NodeSet, Bytes],
            Self::First | Self::Last | Self::Nth => &[List],
            Self::ToString => &[String, Number, Boolean, Null, List, Object, Bytes, Date],
            Self::ToInt | Self::ToFloat => &[String, Number],
            Self::ToHex
            | Self::Base64Encode
            | Self::Base64UrlSafeEncode
            | Self::Utf8Decode
            | Self::CharsetDecode => &[Bytes],
            Self::DateFormat | Self::DaysAfterNow | Self::DaysBeforeNow => &[Date],
            Self::Json => &[String, List, Object],
            Self::Xpath => &[String, Bytes],
            Self::Split
            | Self::Regex
            | Self::Replace
            | Self::ReplaceRegex
            | Self::ToDate
            | Self::Base64Decode
            | Self::Base64UrlSafeDecode
            | Self::Utf8Encode
            | Self::UrlQueryParam
            | Self::UrlEncode
            | Self::UrlDecode
            | Self::HtmlEscape
            | Self::HtmlUnescape => &[String],
        }
    }

    /// The output type for an input type, or `None` when the filter cannot
    /// take that input.
    pub(crate) fn output(self, input: &StaticType) -> Option<StaticType> {
        if !input.accepted_by(self.input_types()) {
            return None;
        }
        Some(match self {
            Self::Count
            | Self::ToInt
            | Self::ToFloat
            | Self::DaysAfterNow
            | Self::DaysBeforeNow => StaticType::NUMBER,
            Self::First | Self::Last | Self::Nth => match input {
                StaticType::ListOf(item) => (**item).clone(),
                _ => StaticType::Any,
            },
            Self::Split => StaticType::ListOf(Box::new(StaticType::STRING)),
            Self::ToDate => StaticType::Known(ValueType::Date),
            Self::Base64Decode | Self::Base64UrlSafeDecode | Self::Utf8Encode => StaticType::BYTES,
            Self::Json | Self::Xpath => StaticType::Any,
            Self::Regex
            | Self::Replace
            | Self::ReplaceRegex
            | Self::ToString
            | Self::ToHex
            | Self::DateFormat
            | Self::Base64Encode
            | Self::Base64UrlSafeEncode
            | Self::Utf8Decode
            | Self::CharsetDecode
            | Self::UrlQueryParam
            | Self::UrlEncode
            | Self::UrlDecode
            | Self::HtmlEscape
            | Self::HtmlUnescape => StaticType::STRING,
        })
    }
}

/// A predicate without its expected value (SPEC 9.4).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum PredicateKind {
    Eq,
    Ne,
    Gt,
    Ge,
    Lt,
    Le,
    StartsWith,
    EndsWith,
    Contains,
    Matches,
    Exists,
    IsBoolean,
    IsEmpty,
    IsFloat,
    IsInteger,
    IsIpv4,
    IsIpv6,
    IsIsoDate,
    IsList,
    IsNumber,
    IsObject,
    IsString,
    IsUuid,
}

/// Predicates that take an expected value, with their spelling.
pub(crate) const COMPARE_KEYWORDS: &[(&str, PredicateKind)] = &[
    ("==", PredicateKind::Eq),
    ("!=", PredicateKind::Ne),
    (">", PredicateKind::Gt),
    (">=", PredicateKind::Ge),
    ("<", PredicateKind::Lt),
    ("<=", PredicateKind::Le),
    ("startsWith", PredicateKind::StartsWith),
    ("endsWith", PredicateKind::EndsWith),
    ("contains", PredicateKind::Contains),
];

/// Predicates that take nothing, with their spelling.
pub(crate) const WORD_PREDICATES: &[(&str, PredicateKind)] = &[
    ("exists", PredicateKind::Exists),
    ("isBoolean", PredicateKind::IsBoolean),
    ("isEmpty", PredicateKind::IsEmpty),
    ("isFloat", PredicateKind::IsFloat),
    ("isInteger", PredicateKind::IsInteger),
    ("isIpv4", PredicateKind::IsIpv4),
    ("isIpv6", PredicateKind::IsIpv6),
    ("isIsoDate", PredicateKind::IsIsoDate),
    ("isList", PredicateKind::IsList),
    ("isNumber", PredicateKind::IsNumber),
    ("isObject", PredicateKind::IsObject),
    ("isString", PredicateKind::IsString),
    ("isUuid", PredicateKind::IsUuid),
];

impl PredicateKind {
    /// The predicate's spelling in `.whirl` source.
    pub(crate) fn name(self) -> &'static str {
        if self == Self::Matches {
            return "matches";
        }
        COMPARE_KEYWORDS
            .iter()
            .chain(WORD_PREDICATES)
            .find(|(_, kind)| *kind == self)
            .map_or("?", |(name, _)| name)
    }

    /// The value types the predicate accepts (SPEC 9.4); `None` means any.
    pub(crate) fn value_types(self) -> Option<&'static [ValueType]> {
        use ValueType::{Bytes, Date, List, Number, Object, String};
        match self {
            Self::Gt | Self::Ge | Self::Lt | Self::Le => Some(&[Number, Date]),
            Self::StartsWith | Self::EndsWith => Some(&[String, Bytes]),
            Self::Contains => Some(&[String, List, Bytes]),
            Self::Matches => Some(&[String]),
            Self::IsEmpty => Some(&[List, Object]),
            _ => None,
        }
    }

    /// True when the predicate can test a value of this static type.
    pub(crate) fn accepts(self, value: &StaticType) -> bool {
        self.value_types()
            .is_none_or(|accepted| value.accepted_by(accepted))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn follows_filters_through_a_chain() {
        let split = FilterKind::Split
            .output(&StaticType::STRING)
            .expect("split takes a string");
        let first = FilterKind::First
            .output(&split)
            .expect("first takes a list");
        assert!(first.is_string());
        assert_eq!(FilterKind::ToInt.output(&first), Some(StaticType::NUMBER));
    }

    #[test]
    fn rejects_filters_on_the_wrong_type() {
        assert_eq!(FilterKind::ToHex.output(&StaticType::STRING), None);
        assert_eq!(FilterKind::Split.output(&StaticType::NUMBER), None);
    }

    #[test]
    fn lets_any_value_through_until_run_time() {
        assert_eq!(
            FilterKind::ToHex.output(&StaticType::Any),
            Some(StaticType::STRING)
        );
        assert_eq!(
            FilterKind::First.output(&StaticType::Any),
            Some(StaticType::Any)
        );
        assert!(!StaticType::Any.is_string());
    }

    #[test]
    fn checks_predicate_types() {
        assert!(!PredicateKind::Gt.accepts(&StaticType::STRING));
        assert!(PredicateKind::Gt.accepts(&StaticType::NUMBER));
        assert!(PredicateKind::Contains.accepts(&StaticType::ListOf(Box::new(StaticType::Any))));
        assert!(PredicateKind::Eq.accepts(&StaticType::STRING));
    }

    #[test]
    fn names_every_keyword() {
        assert_eq!(FilterKind::ReplaceRegex.name(), "replaceRegex");
        assert_eq!(FilterKind::Json.name(), "json:");
        assert_eq!(PredicateKind::Ge.name(), ">=");
        assert_eq!(PredicateKind::IsUuid.name(), "isUuid");
        assert_eq!(PredicateKind::Matches.name(), "matches");
    }
}
